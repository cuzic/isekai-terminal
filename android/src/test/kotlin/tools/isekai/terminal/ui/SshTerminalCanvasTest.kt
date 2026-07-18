package tools.isekai.terminal.ui

import android.graphics.Typeface
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import uniffi.isekai_terminal_core.CellData
import uniffi.isekai_terminal_core.CursorShape
import uniffi.isekai_terminal_core.MouseReportingMode
import uniffi.isekai_terminal_core.ScreenUpdate
import uniffi.isekai_terminal_core.ScrollbackSearchMatch

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

    // ── dimmedArgb: SGR 2(dim)の前景色alpha計算(タスク#22) ──────────────

    @Test
    fun `dimmedArgb scales down the alpha channel while keeping rgb unchanged`() {
        val opaqueWhite = 0xFFFFFFFF.toInt()
        val dimmed = dimmedArgb(opaqueWhite)
        assertEquals(0x00FFFFFF, dimmed and 0x00FFFFFF)
        val dimmedAlpha = (dimmed ushr 24) and 0xFF
        assertEquals(153, dimmedAlpha) // 255 * 0.6 = 153
    }

    @Test
    fun `dimmedArgb never overflows into a negative alpha`() {
        val transparent = 0x00112233
        val dimmed = dimmedArgb(transparent)
        assertEquals(0, (dimmed ushr 24) and 0xFF)
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
        80u, 24u, emptyList(), 0u, 0u, null, false, false, false,
        MouseReportingMode.OFF, false, true, 0uL, CursorShape.BLOCK, true, emptyList(),
        emptyList(), 0u,
    )

    // ── computeCursorRect: DECSCUSR(タスク#33)のblock/underline/bar描画分岐 ──────

    @Test
    fun `computeCursorRect for block fills the entire cell`() {
        val rect = computeCursorRect(cx = 10f, cy = 20f, cellW = 8f, cellH = 16f, shape = CursorShape.BLOCK)
        assertEquals(CursorRect(10f, 20f, 18f, 36f), rect)
    }

    @Test
    fun `computeCursorRect for underline is a thin strip at the cell bottom`() {
        val rect = computeCursorRect(cx = 10f, cy = 20f, cellW = 8f, cellH = 16f, shape = CursorShape.UNDERLINE)
        // cellH * 0.12 = 1.92 < 2px の下限にクランプされる
        assertEquals(CursorRect(10f, 34f, 18f, 36f), rect)
        assertEquals("横幅はセル幅いっぱい", 8f, rect.right - rect.left)
        assertEquals("太さは2px下限でクランプされる", 2f, rect.bottom - rect.top)
    }

    @Test
    fun `computeCursorRect for underline uses proportional thickness when cell is tall`() {
        val rect = computeCursorRect(cx = 0f, cy = 0f, cellW = 8f, cellH = 100f, shape = CursorShape.UNDERLINE)
        assertEquals(12f, rect.bottom - rect.top, 0.01f) // 100 * 0.12 (Float丸め誤差を許容)
    }

    @Test
    fun `computeCursorRect for bar is a thin strip at the cell left edge`() {
        val rect = computeCursorRect(cx = 10f, cy = 20f, cellW = 8f, cellH = 16f, shape = CursorShape.BAR)
        // cellW * 0.15 = 1.2 < 2px の下限にクランプされる
        assertEquals(CursorRect(10f, 20f, 12f, 36f), rect)
        assertEquals("縦幅はセル高さいっぱい", 16f, rect.bottom - rect.top)
        assertEquals("太さは2px下限でクランプされる", 2f, rect.right - rect.left)
    }

    @Test
    fun `computeCursorRect for bar uses proportional thickness when cell is wide`() {
        val rect = computeCursorRect(cx = 0f, cy = 0f, cellW = 100f, cellH = 16f, shape = CursorShape.BAR)
        assertEquals(15f, rect.right - rect.left, 0.01f) // 100 * 0.15 (Float丸め誤差を許容)
    }

    // ── computeSearchHighlightRect: スクロールバック検索(タスク#66)のハイライト矩形計算 ──

    @Test
    fun `computeSearchHighlightRect places the highlight on the last row`() {
        val match = ScrollbackSearchMatch(row = 3u, col = 2u, len = 4u)
        val rect = computeSearchHighlightRect(match, rows = 24, cols = 80, cellW = 8f, cellH = 16f)
        assertEquals(CursorRect(16f, 23 * 16f, 48f, 24 * 16f), rect)
    }

    @Test
    fun `computeSearchHighlightRect clamps a match that overflows past the right edge`() {
        // colを超えてはみ出すマッチ(col + len > cols)——クランプされるだけでクラッシュしない
        // ことを確認する(iOS版TerminalScreenViewTestsの
        // testDrawWithSearchHighlightMatchingScrollOffsetDoesNotCrashと同種のケース)。
        val match = ScrollbackSearchMatch(row = 0u, col = 3u, len = 10u)
        val rect = computeSearchHighlightRect(match, rows = 2, cols = 4, cellW = 8f, cellH = 16f)
        assertEquals(CursorRect(24f, 16f, 32f, 32f), rect)
    }

    @Test
    fun `computeSearchHighlightRect returns null when the match starts past the last column`() {
        val match = ScrollbackSearchMatch(row = 0u, col = 10u, len = 1u)
        val rect = computeSearchHighlightRect(match, rows = 2, cols = 4, cellW = 8f, cellH = 16f)
        assertNull("画面外のマッチは描画対象なし(null)を返すこと", rect)
    }

    @Test
    fun `computeSearchHighlightRect returns null for a zero-length match`() {
        val match = ScrollbackSearchMatch(row = 0u, col = 1u, len = 0u)
        val rect = computeSearchHighlightRect(match, rows = 2, cols = 4, cellW = 8f, cellH = 16f)
        assertNull(rect)
    }

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
        assertTrue(cache.needsRerender(screenUpdate(), 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false))
    }

    @Test
    fun `GridRenderCache does not require rerender when nothing changed`() {
        val cache = GridRenderCache()
        val update = screenUpdate()
        cache.markRendered(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)
        assertFalse(cache.needsRerender(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false))
    }

    @Test
    fun `GridRenderCache requires rerender when only typeface changes`() {
        val cache = GridRenderCache()
        val update = screenUpdate()
        cache.markRendered(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)
        assertTrue(
            "フォント変更後も同じScreenUpdate/サイズなら古いフォントで描いたBitmapが再利用されてしまう",
            cache.needsRerender(update, 10f, 20f, 0xFF000000.toInt(), Typeface.SERIF, false),
        )
    }

    @Test
    fun `GridRenderCache requires rerender when a new ScreenUpdate instance arrives`() {
        val cache = GridRenderCache()
        cache.markRendered(screenUpdate(), 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)
        assertTrue(cache.needsRerender(screenUpdate(), 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false))
    }

    @Test
    fun `GridRenderCache invalidate forces rerender even if nothing else changed`() {
        val cache = GridRenderCache()
        val update = screenUpdate()
        cache.markRendered(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)
        cache.invalidate()
        assertTrue(cache.needsRerender(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false))
    }

    @Test
    fun `GridRenderCache requires rerender when only blink phase changes`() {
        // Fableレビュー2次で指摘された罠: ScreenUpdate自体は変わらずblink位相だけが
        // 反転するケースでも、キャッシュキーにblinkPhaseを含めていないと再描画されず
        // 「一度描かれたきり点滅しない」バグになる。
        val cache = GridRenderCache()
        val update = screenUpdate()
        cache.markRendered(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, blinkPhase = false)
        assertTrue(
            "blink位相の反転だけでは他のキーが変わらないため、blinkPhaseをキーに含めないと再描画がスキップされてしまう",
            cache.needsRerender(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, blinkPhase = true),
        )
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
