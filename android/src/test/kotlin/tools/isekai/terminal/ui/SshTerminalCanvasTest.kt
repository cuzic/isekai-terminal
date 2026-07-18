package tools.isekai.terminal.ui

import android.graphics.Typeface
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import uniffi.isekai_terminal_core.CellData
import uniffi.isekai_terminal_core.CursorShape
import uniffi.isekai_terminal_core.MouseReportingMode
import uniffi.isekai_terminal_core.ScreenUpdate

/**
 * [computeBgRuns] は背景描画のバッチ化(セルごとの `drawRect` を連続区間ごとの
 * `drawRect` にまとめる)のためのピュア関数。実際のレンダリング結果と等価であることを
 * 「区間の和集合がセルごとの塗りつぶし対象と一致する」形で検証する。
 *
 * [FontFitCache]/[GridRenderCache] は、性能改善(背景描画バッチ化とフォント計測キャッシュ)
 * のために追加したキャッシュのinvalidation条件(セル寸法・テーマ背景色・typefaceの
 * いずれかが変わったら再計算/再描画する)を検証する。実際の描画結果ではなく
 * 「再計算/再描画が必要と判断されるかどうか」だけをピュアに検証できるよう、
 * `needsRefit`/`needsRerender` はComposeの`Canvas{}`スコープの外から直接呼べる。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class SshTerminalCanvasTest {

    private val defaultBg = 0xFF000000.toInt()

    private fun cell(bg: Int) = CellData(
        ch = " ", fg = 0xFFFFFFFFu, bg = bg.toUInt(), bold = false,
        dim = false, italic = false, underline = false,
        strikethrough = false, blink = false, invisible = false,
        linkId = null,
    )

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

    private fun screenUpdate() = ScreenUpdate(
        80u, 24u, emptyList(), 0u, 0u, null, false, false,
        MouseReportingMode.OFF, false, true, 0uL, CursorShape.BLOCK, true, emptyList(),
        emptyList(), 0u,
    )

    // ── FontFitCache: セル寸法/typefaceが変わったときだけ再計測が必要 ──────────

    @Test
    fun `FontFitCache requires refit on first use`() {
        val cache = FontFitCache()
        assertTrue(cache.needsRefit(10f, 20f, Typeface.MONOSPACE))
    }

    @Test
    fun `FontFitCache does not require refit when cell size and typeface are unchanged`() {
        val cache = FontFitCache()
        cache.markFit(10f, 20f, Typeface.MONOSPACE, baseline = 15f)
        assertFalse(cache.needsRefit(10f, 20f, Typeface.MONOSPACE))
    }

    @Test
    fun `FontFitCache requires refit when only typeface changes`() {
        val cache = FontFitCache()
        cache.markFit(10f, 20f, Typeface.MONOSPACE, baseline = 15f)
        assertTrue("フォント変更後は古いcellW/cellHのままtextSize/baselineが再計算されないと壊れる", cache.needsRefit(10f, 20f, Typeface.SERIF))
    }

    @Test
    fun `FontFitCache requires refit when only cell size changes`() {
        val cache = FontFitCache()
        cache.markFit(10f, 20f, Typeface.MONOSPACE, baseline = 15f)
        assertTrue(cache.needsRefit(11f, 20f, Typeface.MONOSPACE))
    }

    // ── GridRenderCache: update参照/セル寸法/テーマ背景/typefaceが変わったときだけ再描画 ──

    @Test
    fun `GridRenderCache requires rerender on first use`() {
        val cache = GridRenderCache()
        assertTrue(cache.needsRerender(screenUpdate(), 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE))
    }

    @Test
    fun `GridRenderCache does not require rerender when nothing changed`() {
        val cache = GridRenderCache()
        val update = screenUpdate()
        cache.markRendered(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE)
        assertFalse(cache.needsRerender(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE))
    }

    @Test
    fun `GridRenderCache requires rerender when only typeface changes`() {
        val cache = GridRenderCache()
        val update = screenUpdate()
        cache.markRendered(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE)
        assertTrue(
            "フォント変更後も同じScreenUpdate/サイズなら古いフォントで描いたBitmapが再利用されてしまう",
            cache.needsRerender(update, 10f, 20f, 0xFF000000.toInt(), Typeface.SERIF),
        )
    }

    @Test
    fun `GridRenderCache requires rerender when a new ScreenUpdate instance arrives`() {
        val cache = GridRenderCache()
        cache.markRendered(screenUpdate(), 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE)
        assertTrue(cache.needsRerender(screenUpdate(), 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE))
    }

    @Test
    fun `GridRenderCache invalidate forces rerender even if nothing else changed`() {
        val cache = GridRenderCache()
        val update = screenUpdate()
        cache.markRendered(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE)
        cache.invalidate()
        assertTrue(cache.needsRerender(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE))
    }

    // ── SixelBitmapCache(タスク#42) ──────────────────────────────

    private fun imagePlacement(id: ULong, w: Int = 1, h: Int = 1) = uniffi.isekai_terminal_core.ImagePlacement(
        id = id, row = 0u, col = 0u, rowsSpan = 1u, colsSpan = 1u,
        widthPx = w.toUInt(), heightPx = h.toUInt(),
        rgba = ByteArray(w * h * 4) { 0xFF.toByte() },
    )

    @Test
    fun `SixelBitmapCache decodes a bitmap for each distinct id`() {
        val cache = SixelBitmapCache()
        val images = listOf(imagePlacement(1u), imagePlacement(2u))
        val bitmaps = cache.bitmapsFor(images)
        assertEquals(2, bitmaps.size)
        assertTrue(bitmaps.containsKey(1u.toULong()))
        assertTrue(bitmaps.containsKey(2u.toULong()))
    }

    @Test
    fun `SixelBitmapCache reuses the same Bitmap instance for an id seen again`() {
        val cache = SixelBitmapCache()
        val placement = imagePlacement(1u)
        val first = cache.bitmapsFor(listOf(placement))[1u.toULong()]
        val second = cache.bitmapsFor(listOf(placement))[1u.toULong()]
        assertTrue("同じidなら再デコードせず同一Bitmapインスタンスを返すこと", first === second)
    }

    @Test
    fun `SixelBitmapCache drops entries whose id is no longer live`() {
        val cache = SixelBitmapCache()
        cache.bitmapsFor(listOf(imagePlacement(1u), imagePlacement(2u)))
        val after = cache.bitmapsFor(listOf(imagePlacement(2u)))
        assertEquals(
            "Rust側のScreenUpdate.imagesに出てこなくなったidはキャッシュから捨てられること",
            setOf(2u.toULong()),
            after.keys,
        )
    }
}
