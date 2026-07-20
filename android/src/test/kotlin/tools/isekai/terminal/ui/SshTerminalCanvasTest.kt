package tools.isekai.terminal.ui

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.PorterDuff
import android.graphics.PorterDuffXfermode
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
import uniffi.isekai_terminal_core.LineDamage
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
        0u, 80u, 24u, emptyList(), 0u, 0u, null, false, false, false,
        MouseReportingMode.OFF, false, true, 0uL, CursorShape.BLOCK, true, emptyList(),
        emptyList(), 0u, null,
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

    // ── GridRenderCache.planRender: dirty_rows に基づく Full/Partial/Reuse 判定(タスク#97) ──

    @Test
    fun `planRender returns Full when dirty_rows is null even if style unchanged`() {
        val cache = GridRenderCache()
        val first = screenUpdate() // updateSeq=0, dirtyRows = null
        cache.markRendered(first, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)
        // 連番は途切れていない(ギャップではない)が dirtyRows=null なので全画面dirty=Full。
        val next = screenUpdate().copy(updateSeq = 1u)
        assertEquals(
            GridRenderPlan.Full,
            cache.planRender(next, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false),
        )
    }

    @Test
    fun `planRender returns Full when style changes regardless of dirty_rows`() {
        val cache = GridRenderCache()
        val first = screenUpdate()
        cache.markRendered(first, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)
        // dirty_rows は行1だけを指すが、セル寸法が変わっているので Partial ではなく Full。
        val next = screenUpdate().copy(dirtyRows = listOf(LineDamage(1u, 0u, 3u)))
        assertEquals(
            GridRenderPlan.Full,
            cache.planRender(next, 11f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false),
        )
    }

    @Test
    fun `planRender returns Reuse for the same instance already rendered`() {
        val cache = GridRenderCache()
        val update = screenUpdate()
        cache.markRendered(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)
        assertEquals(
            GridRenderPlan.Reuse,
            cache.planRender(update, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false),
        )
    }

    @Test
    fun `planRender returns Partial with only the dirty rows when style unchanged`() {
        val cache = GridRenderCache()
        val first = screenUpdate()
        cache.markRendered(first, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)
        val next = screenUpdate().copy(
            updateSeq = 1u, // first(=0)の次。連番が連続していればdirty_rowsに従う。
            dirtyRows = listOf(LineDamage(2u, 0u, 5u), LineDamage(7u, 1u, 3u)),
        )
        assertEquals(
            GridRenderPlan.Partial(listOf(2, 7)),
            cache.planRender(next, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false),
        )
    }

    @Test
    fun `planRender forces Full when DirtyRowDebugFlags forceFullRedraw is set`() {
        // タスク#100: dirty行の見落としは表示バグとして気づきにくいため、実機/CIで
        // すぐ旧経路(常に全画面再描画)へ切り戻せるデバッグトグルを検証する。
        val cache = GridRenderCache()
        val first = screenUpdate()
        cache.markRendered(first, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)
        val next = screenUpdate().copy(
            updateSeq = 1u,
            dirtyRows = listOf(LineDamage(2u, 0u, 5u)),
        )
        DirtyRowDebugFlags.forceFullRedraw = true
        try {
            assertEquals(
                "トグルが有効な間はdirty_rowsを無視して常にFullになるべき",
                GridRenderPlan.Full,
                cache.planRender(next, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false),
            )
        } finally {
            DirtyRowDebugFlags.forceFullRedraw = false
        }
    }

    @Test
    fun `planRender Partial filters out-of-range rows and de-duplicates`() {
        val cache = GridRenderCache()
        val first = screenUpdate() // 24行
        cache.markRendered(first, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)
        val next = screenUpdate().copy(
            updateSeq = 1u,
            // 行3が重複、行99は画面外(24行なので除外されるべき)。
            dirtyRows = listOf(LineDamage(3u, 0u, 1u), LineDamage(3u, 2u, 4u), LineDamage(99u, 0u, 1u)),
        )
        assertEquals(
            GridRenderPlan.Partial(listOf(3)),
            cache.planRender(next, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false),
        )
    }

    @Test
    fun `planRender returns empty Partial when dirty_rows is empty`() {
        val cache = GridRenderCache()
        val first = screenUpdate()
        cache.markRendered(first, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)
        val next = screenUpdate().copy(updateSeq = 1u, dirtyRows = emptyList())
        assertEquals(
            GridRenderPlan.Partial(emptyList()),
            cache.planRender(next, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false),
        )
    }

    @Test
    fun `planRender falls back to Full when updateSeq skips a frame even if dirty_rows is present`() {
        // 配信チャネル(Channel.CONFLATED)が中間の発行を取りこぼすと、updateSeqが+1で連続せず
        // 飛ぶ。この場合 dirty_rows は欠落分の変化を含まないため信用できず、強制的にFullへ
        // フォールバックしなければ「取りこぼした行が古いまま残る」表示化けになる(Rust側 update_seq
        // 追加のUI側対応)。
        val cache = GridRenderCache()
        val first = screenUpdate().copy(updateSeq = 5u)
        cache.markRendered(first, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)

        // updateSeq が 6 ではなく 7(=1つ飛んでいる)。dirty_rows は行1だけだが Full になるべき。
        val skipped = screenUpdate().copy(updateSeq = 7u, dirtyRows = listOf(LineDamage(1u, 0u, 3u)))
        assertEquals(
            "updateSeqが飛んだらdirty_rowsがSomeでも全画面再描画にフォールバックすべき",
            GridRenderPlan.Full,
            cache.planRender(skipped, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false),
        )

        // 対照: 連番が連続(6)していれば同じ dirty_rows でも Partial のままであること。
        val cache2 = GridRenderCache()
        cache2.markRendered(screenUpdate().copy(updateSeq = 5u), 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)
        val consecutive = screenUpdate().copy(updateSeq = 6u, dirtyRows = listOf(LineDamage(1u, 0u, 3u)))
        assertEquals(
            GridRenderPlan.Partial(listOf(1)),
            cache2.planRender(consecutive, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false),
        )
    }

    @Test
    fun `planRender handles updateSeq wrap-around without a false gap`() {
        // updateSeq は wrapping_add。UInt.MAX の次は 0 で、これは正常な連番でありギャップではない。
        val cache = GridRenderCache()
        cache.markRendered(screenUpdate().copy(updateSeq = UInt.MAX_VALUE), 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false)
        val wrapped = screenUpdate().copy(updateSeq = 0u, dirtyRows = listOf(LineDamage(2u, 0u, 3u)))
        assertEquals(
            "UInt.MAX の次の 0 はラップした正常な連番なのでギャップ扱いしないこと",
            GridRenderPlan.Partial(listOf(2)),
            cache.planRender(wrapped, 10f, 20f, 0xFF000000.toInt(), Typeface.MONOSPACE, false),
        )
    }

    // ── redrawDirtyRows: 部分再描画時の行クリア(タスク#104) ─────────────────
    //
    // 部分再描画で dirty 行を描き直すとき、その行が前フレームで持っていた背景色/グリフの
    // 残骸が完全に消える(消し残しが無い)こと、かつ dirty でない行のピクセルは一切
    // 変化しないことをピクセル単位で検証する。iOS版 #98/#99 の部分invalidate検証(#103)と対称。

    private fun charCell(ch: String, fg: Int, bg: Int) = CellData(
        ch = ch, fg = fg.toUInt(), bg = bg.toUInt(), bold = false,
        dim = false, italic = false, underline = false,
        strikethrough = false, blink = false, invisible = false,
        linkId = null,
    )

    /** テスト用の Paint 一式(本番 [SshTerminalCanvas] と同じ構成)。 */
    private data class RenderPaints(
        val clearPaint: Paint,
        val bgPaint: Paint,
        val textPaint: Paint,
        val typeface: Typeface,
        val italicTypeface: Typeface,
        val baseline: Float,
    )

    private fun renderPaints(cellH: Float): RenderPaints {
        val typeface = Typeface.MONOSPACE
        val textPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            this.typeface = typeface
            textSize = cellH * 0.75f
        }
        return RenderPaints(
            clearPaint = Paint().apply { xfermode = PorterDuffXfermode(PorterDuff.Mode.CLEAR) },
            bgPaint = Paint(),
            textPaint = textPaint,
            typeface = typeface,
            italicTypeface = Typeface.create(typeface, Typeface.ITALIC),
            baseline = -textPaint.fontMetrics.top,
        )
    }

    /** 透明 Bitmap に全行を(部分再描画と同じ経路で)描き切って「基準/前フレーム」を作る。 */
    private fun fullRender(cells: List<CellData>, cols: Int, rows: Int, cellW: Float, cellH: Float): Bitmap {
        val bmp = Bitmap.createBitmap((cols * cellW).toInt(), (rows * cellH).toInt(), Bitmap.Config.ARGB_8888)
        bmp.eraseColor(android.graphics.Color.TRANSPARENT)
        val p = renderPaints(cellH)
        redrawDirtyRows(
            canvas = Canvas(bmp), rows = (0 until rows).toList(), bitmapWidthPx = bmp.width,
            cells = cells, cols = cols, cellW = cellW, cellH = cellH, baseline = p.baseline,
            themeBgArgb = 0xFF000000.toInt(), effectiveBlinkPhase = false,
            clearPaint = p.clearPaint, bgPaint = p.bgPaint, textPaint = p.textPaint,
            typeface = p.typeface, italicTypeface = p.italicTypeface,
            glyphFallback = GlyphFallbackResolver(),
        )
        return bmp
    }

    private fun Bitmap.rowPixels(row: Int, cols: Int, cellW: Float, cellH: Float): IntArray {
        val w = (cols * cellW).toInt()
        val h = cellH.toInt()
        val out = IntArray(w * h)
        getPixels(out, 0, w, 0, (row * cellH).toInt(), w, h)
        return out
    }

    @Test
    fun `redrawDirtyRows clears stale background and glyph on the re-rendered row`() {
        // 5行グリッド。dirty 行は中央(行2)。近傍の行1・行3はグリフの縦方向アンチエイリアスが
        // 1px 滲む可能性があるため「非dirty行が保持されること」の検証には非隣接の行0・行4を使う
        // (グリフ高さ ≪ 行間隔なので確実に滲みが届かない)。行0・行4は無地の背景色(drawRectのみ、
        // アンチエイリアス無し)にして厳密一致で検証する。
        val cols = 4
        val rows = 5
        val cellW = 10f
        val cellH = 16f
        val red = 0xFFFF0000.toInt()
        val blue = 0xFF0000FF.toInt()
        val green = 0xFF00FF00.toInt()
        val yellow = 0xFFFFFF00.toInt()
        val white = 0xFFFFFFFFu.toInt()
        val defaultBg = 0xFF000000.toInt()

        fun row(bg: Int, ch: String) = List(cols) { charCell(ch, white, bg) }
        // frame1: 行0=緑, 行1=既定, 行2=赤背景+"X", 行3=既定, 行4=黄。
        val frame1 = row(green, " ") + row(defaultBg, " ") + row(red, "X") + row(defaultBg, " ") + row(yellow, " ")
        // frame2: 行2 だけが 青背景+"Y" に変化。他の行は frame1 と同一。
        val frame2 = row(green, " ") + row(defaultBg, " ") + row(blue, "Y") + row(defaultBg, " ") + row(yellow, " ")

        val base = fullRender(frame1, cols, rows, cellW, cellH)
        val row0Before = base.rowPixels(0, cols, cellW, cellH)
        val row4Before = base.rowPixels(4, cols, cellW, cellH)
        val row2Frame1 = base.rowPixels(2, cols, cellW, cellH)

        // 行2 だけを部分再描画(dirty_rows=[2])。
        val p = renderPaints(cellH)
        redrawDirtyRows(
            canvas = Canvas(base), rows = listOf(2), bitmapWidthPx = base.width,
            cells = frame2, cols = cols, cellW = cellW, cellH = cellH, baseline = p.baseline,
            themeBgArgb = defaultBg, effectiveBlinkPhase = false,
            clearPaint = p.clearPaint, bgPaint = p.bgPaint, textPaint = p.textPaint,
            typeface = p.typeface, italicTypeface = p.italicTypeface,
            glyphFallback = GlyphFallbackResolver(),
        )

        val row2After = base.rowPixels(2, cols, cellW, cellH)

        // 検証1(残骸消去): 行2に赤背景(frame1)の残骸が1ピクセルも残らず、新しい青で塗られ、
        // 前フレームの行2(赤+"X")とはピクセルが異なること。
        assertFalse("部分再描画後の行2が前フレーム(赤+X)のままではないこと", row2After.contentEquals(row2Frame1))
        assertFalse("行2に赤背景の残骸が残っていないこと", row2After.any { it == red })
        assertTrue("行2の背景は新しい青で塗られていること", row2After.any { it == blue })

        // 検証2(dirty行の内容が基準の全描画と一致): frame2 を最初から全描画したときの行2バンドと、
        // 部分再描画後の行2バンドが完全一致する(クリア→再描画で "X" の残骸なく "Y" が描かれている)。
        val reference = fullRender(frame2, cols, rows, cellW, cellH)
        val row2Reference = reference.rowPixels(2, cols, cellW, cellH)
        assertTrue(
            "部分再描画後の行2は frame2 を全描画した行2と完全一致すべき(残骸消去 + 新内容描画)",
            row2After.contentEquals(row2Reference),
        )

        // 検証3(非dirty行の保持): dirty に含まれない行0(緑)・行4(黄)のピクセルは部分再描画で
        // 一切変化しない。
        assertTrue("dirtyでない行0のピクセルは部分再描画で変化しないこと", row0Before.contentEquals(base.rowPixels(0, cols, cellW, cellH)))
        assertTrue("dirtyでない行4のピクセルは部分再描画で変化しないこと", row4Before.contentEquals(base.rowPixels(4, cols, cellW, cellH)))
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

    // タスク#76: Rust側`ImagePlacement.rgba`はRGBA8888バイト順で詰められているが、
    // `Bitmap.copyPixelsFromBuffer`で生コピーすると内部バイトレイアウト次第で赤/青
    // チャンネルが入れ替わりうる(codex/Fableレビュー指摘)。赤1px・青1pxのRGBAバイト列を
    // デコードし、`getPixel`が正しいARGB値(チャンネル入れ替わり無し)を返すことを検証する。
    @Test
    fun `SixelBitmapCache decodes red and blue pixels without channel swap`() {
        val cache = SixelBitmapCache()
        // R=0xFF,G=0x00,B=0x00,A=0xFF(赤、不透明)
        val red = uniffi.isekai_terminal_core.ImagePlacement(
            id = 1u, row = 0u, col = 0u, rowsSpan = 1u, colsSpan = 1u,
            widthPx = 1u, heightPx = 1u,
            rgba = byteArrayOf(0xFF.toByte(), 0x00, 0x00, 0xFF.toByte()),
        )
        // R=0x00,G=0x00,B=0xFF,A=0xFF(青、不透明)
        val blue = uniffi.isekai_terminal_core.ImagePlacement(
            id = 2u, row = 0u, col = 0u, rowsSpan = 1u, colsSpan = 1u,
            widthPx = 1u, heightPx = 1u,
            rgba = byteArrayOf(0x00, 0x00, 0xFF.toByte(), 0xFF.toByte()),
        )
        val bitmaps = cache.bitmapsFor(listOf(red, blue))

        val redPixel = bitmaps.getValue(1u.toULong()).getPixel(0, 0)
        assertEquals(0xFFFF0000.toInt(), redPixel)

        val bluePixel = bitmaps.getValue(2u.toULong()).getPixel(0, 0)
        assertEquals(0xFF0000FF.toInt(), bluePixel)
    }

    // `setPixels`変換ヘルパー単体でも、複数ピクセル・半透明を含めた変換が正しいことを検証する。
    @Test
    fun `rgbaBytesToArgbInts converts multiple pixels including alpha`() {
        val rgba = byteArrayOf(
            0xFF.toByte(), 0x00, 0x00, 0xFF.toByte(), // 赤、不透明
            0x00, 0xFF.toByte(), 0x00, 0x80.toByte(), // 緑、半透明
        )
        val pixels = rgbaBytesToArgbInts(rgba)
        assertEquals(2, pixels.size)
        assertEquals(0xFFFF0000.toInt(), pixels[0])
        assertEquals(0x8000FF00.toInt(), pixels[1])
    }

    // ── computeLineDecorationRects: underline/strikethrough(SGR 4/9)の装飾線矩形計算 ──
    //
    // `Paint.isUnderlineText`/`isStrikeThruText`は、空白のみの文字列に対して
    // Robolectric実描画でも実際には装飾線を描かないことが確認された(iOS版
    // `NSAttributedString.underlineStyle`が空白セルでCoreTextにより描画されない
    // 実機バグ[コミット`5da238e`]と対称のAndroid側の不具合)。このため本番コードは
    // `computeLineDecorationRects`が返すRectを`drawRect`で直接塗る方式に置き換えた。

    @Test
    fun `computeLineDecorationRects returns nothing when neither flag is set`() {
        val rects = computeLineDecorationRects(x = 0f, y = 0f, cellW = 8f, cellH = 16f, underline = false, strikethrough = false)
        assertTrue(rects.isEmpty())
    }

    @Test
    fun `computeLineDecorationRects underline is a thin strip at the cell bottom`() {
        val rects = computeLineDecorationRects(x = 10f, y = 20f, cellW = 8f, cellH = 16f, underline = true, strikethrough = false)
        assertEquals(1, rects.size)
        val rect = rects.single()
        assertEquals("横幅はセル幅いっぱい", 8f, rect.right - rect.left)
        assertEquals("下端はセル下端に一致", 36f, rect.bottom)
        assertTrue("上端は下端より上", rect.top < rect.bottom)
    }

    @Test
    fun `computeLineDecorationRects strikethrough is centered vertically`() {
        val rects = computeLineDecorationRects(x = 10f, y = 20f, cellW = 8f, cellH = 16f, underline = false, strikethrough = true)
        assertEquals(1, rects.size)
        val rect = rects.single()
        val midY = 20f + 16f * 0.5f
        assertTrue("中央線を挟んで配置される", rect.top < midY && rect.bottom > midY)
    }

    @Test
    fun `computeLineDecorationRects returns both rects when both flags are set`() {
        val rects = computeLineDecorationRects(x = 0f, y = 0f, cellW = 8f, cellH = 16f, underline = true, strikethrough = true)
        assertEquals(2, rects.size)
    }

    // iOS版`TerminalScreenViewTests.swift`の
    // `testUnderlineAndStrikethroughOnBlankCellAffectRenderedPixels`と対称の統合テスト。
    // `computeLineDecorationRects`が返す矩形を実際に`Canvas.drawRect`で描き、装飾なしの
    // 空白セルとピクセルが実際に異なることを確認する(`drawRect`は`Paint.isUnderlineText`と
    // 異なり空文字列の有無に依存しない単純な塗りつぶしのため、この経路なら確実に描画される)。
    @Test
    fun `computeLineDecorationRects rects actually change rendered pixels for a blank space`() {
        fun renderedPixels(underline: Boolean, strikethrough: Boolean): IntArray {
            val bmp = Bitmap.createBitmap(40, 40, Bitmap.Config.ARGB_8888)
            val canvas = Canvas(bmp)
            canvas.drawColor(0xFF000000.toInt())
            val paint = Paint(Paint.ANTI_ALIAS_FLAG)
            paint.color = 0xFFFFFFFF.toInt()
            for (rect in computeLineDecorationRects(x = 0f, y = 0f, cellW = 40f, cellH = 40f, underline, strikethrough)) {
                canvas.drawRect(rect.left, rect.top, rect.right, rect.bottom, paint)
            }
            val pixels = IntArray(40 * 40)
            bmp.getPixels(pixels, 0, 40, 0, 0, 40, 40)
            return pixels
        }

        val plain = renderedPixels(underline = false, strikethrough = false)
        val underlined = renderedPixels(underline = true, strikethrough = false)
        val struck = renderedPixels(underline = false, strikethrough = true)

        assertFalse(
            "underline付きの空白セルは装飾なしの空白セルと異なるピクセルになるはず(タスク#71相当)",
            plain.contentEquals(underlined),
        )
        assertFalse(
            "strikethrough付きの空白セルも装飾なしの空白セルと異なるピクセルになるはず",
            plain.contentEquals(struck),
        )
    }
}
