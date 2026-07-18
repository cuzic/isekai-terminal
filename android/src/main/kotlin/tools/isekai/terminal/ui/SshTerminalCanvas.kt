package tools.isekai.terminal.ui

import android.graphics.Bitmap
import android.graphics.Paint
import android.graphics.Typeface
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import kotlin.math.ceil
import kotlinx.coroutines.delay
import uniffi.isekai_terminal_core.CellData
import uniffi.isekai_terminal_core.CursorShape
import uniffi.isekai_terminal_core.ImagePlacement
import uniffi.isekai_terminal_core.ScreenUpdate
import uniffi.isekai_terminal_core.ScrollbackSearchMatch

/**
 * 1行内で背景色が [themeBgArgb](既定背景)以外の値で連続しているセル区間。
 * 背景の `drawRect` をセル単位ではなく区間単位にまとめてバッチ描画するために使う。
 *
 * 文字(`drawText`)はあえて同様にバッチ化していない: 各セルの `x` 位置はセル幅と
 * フォントの実測グリフ幅が厳密に一致する前提でまとめて描画すると、両者がわずかでも
 * 食い違う場合(このファイルのフォントサイズ調整は "M" の実測幅で近似しているだけで
 * 一致を強制していない)に複数文字目以降の位置が右へ流れてズレる。特に日本語の
 * 全角文字が混じる行で目立ちやすいリスクのため、安全側に倒してセル単位のまま
 * `drawText` している。
 */
internal data class BgRun(val startCol: Int, val endColExclusive: Int, val argb: Int)

/** カーソル描画矩形(ピクセル座標)。[computeCursorRect] の戻り値。 */
internal data class CursorRect(val left: Float, val top: Float, val right: Float, val bottom: Float)

/**
 * タスク#33: DECSCUSR(`CSI Ps SP q`)が選択したカーソル形状([shape]、Rust側`Terminal`
 * が真値を保持、rust-ssot)に応じたカーソル描画矩形をピュアに計算する。`(cx, cy)`は
 * カーソルセル左上のピクセル座標、`cellW`/`cellH`はセル寸法。block はセル全体、
 * underline/bar は最小太さ2px(iOS版`TerminalScreenView.swift`の`switch update.cursorShape`
 * と対称の太さ計算: underlineは`cellH * 0.12`、barは`cellW * 0.15`)を下限に描く。
 */
internal fun computeCursorRect(cx: Float, cy: Float, cellW: Float, cellH: Float, shape: CursorShape): CursorRect =
    when (shape) {
        CursorShape.BLOCK -> CursorRect(cx, cy, cx + cellW, cy + cellH)
        CursorShape.UNDERLINE -> {
            val thickness = (cellH * 0.12f).coerceAtLeast(2f)
            CursorRect(cx, cy + cellH - thickness, cx + cellW, cy + cellH)
        }
        CursorShape.BAR -> {
            val thickness = (cellW * 0.15f).coerceAtLeast(2f)
            CursorRect(cx, cy, cx + thickness, cy + cellH)
        }
    }

/**
 * タスク#66: 検索バーの現在マッチ([match])の描画矩形をピュアに計算する。呼び出し元
 * ([SshTerminalCanvas])は`scrollOffset`が`match.row`と一致する場合にのみ[match]を渡す
 * (=表示中のscrollback合成画面の最終行(`row = rows - 1`)に必ず現れる、`scrollbackCells`
 * の`sb_idx = offset + (rows-1-r)`で`r = rows-1`のとき`sb_idx == offset`になることから
 * 導ける、`session.rs`の`scrollback_cells_orders_oldest_to_newest_top_to_bottom`テスト参照)
 * ため、この関数自体は行位置の判断を行わず矩形計算のみを担う。
 *
 * `match.col`/`match.len`は呼び出し時点のscrollbackスナップショットに基づく値([rows]/[cols]
 * が変化した後の古いマッチ等)であり得るため、`[0, cols]`へクランプする(iOS版
 * `TerminalScreenView.swift`の`min(...)`クランプと対称)。クランプ後に幅が0以下になる
 * (=マッチが完全に画面外にはみ出している)場合は`null`を返し、呼び出し元は描画を
 * スキップしてよい。
 */
internal fun computeSearchHighlightRect(match: ScrollbackSearchMatch, rows: Int, cols: Int, cellW: Float, cellH: Float): CursorRect? {
    val highlightRow = rows - 1
    val startCol = match.col.toInt().coerceIn(0, cols)
    val endCol = (startCol + match.len.toInt()).coerceIn(startCol, cols)
    if (startCol >= endCol) return null
    val y = highlightRow * cellH
    return CursorRect(startCol * cellW, y, endCol * cellW, y + cellH)
}

/**
 * SGR 2(dim)セルの前景色。色そのものを別値へ再計算するのではなく、ARGB の alpha
 * チャンネルを 0.6 倍に下げるだけに留める(iOS版`TerminalScreenView.swift`の
 * `withAlphaComponent(0.6)`と対称)。実際の減光は同じ Bitmap 上に既に描画済みの
 * 背景色との合成で表現される——`bg`側で別の実効色を計算し直す必要は無い。
 */
internal fun dimmedArgb(argb: Int): Int {
    val alpha = ((argb ushr 24) and 0xFF)
    val dimmedAlpha = (alpha * 0.6f).toInt().coerceIn(0, 255)
    return (argb and 0x00FFFFFF) or (dimmedAlpha shl 24)
}

/** [cells] のうち [rowStart] から始まる1行分(長さ [cols])を背景色の連続区間へ分割する。 */
internal fun computeBgRuns(cells: List<CellData>, rowStart: Int, cols: Int, themeBgArgb: Int): List<BgRun> {
    val runs = mutableListOf<BgRun>()
    var runStart = -1
    var runArgb = 0
    for (col in 0 until cols) {
        val bg = cells[rowStart + col].bg.toInt()
        if (bg == themeBgArgb) {
            if (runStart != -1) {
                runs += BgRun(runStart, col, runArgb)
                runStart = -1
            }
        } else if (runStart == -1) {
            runStart = col
            runArgb = bg
        } else if (bg != runArgb) {
            runs += BgRun(runStart, col, runArgb)
            runStart = col
            runArgb = bg
        }
    }
    if (runStart != -1) runs += BgRun(runStart, cols, runArgb)
    return runs
}

/**
 * セル寸法(cellW/cellH)・フォント種別ごとにフォントサイズ・ベースライン計算をキャッシュする。
 * `measureText`/`fontMetrics` は cellW/cellH/typeface が変わらない限り結果が変わらないが、
 * 以前は Canvas が再描画されるたび(＝Rust から ScreenUpdate 通知が来るたび)無条件に
 * 再計算していた。typeface はカスタム端末フォント切り替え時に変わるため、キャッシュキーに
 * 含めないと、フォント変更後も古い cellW/cellH のまま textSize/baseline が再計算されず、
 * 新しい Paint に正しくないサイズが残ってしまう。
 */
internal class FontFitCache {
    private var cellW = -1f
    private var cellH = -1f
    private var typeface: Typeface? = null
    var baseline = 0f
        private set

    /** [cellW]/[cellH]/[typeface] のいずれかが前回計算時と変わっていれば true。 */
    fun needsRefit(cellW: Float, cellH: Float, typeface: Typeface): Boolean =
        this.cellW != cellW || this.cellH != cellH || this.typeface !== typeface

    fun markFit(cellW: Float, cellH: Float, typeface: Typeface, baseline: Float) {
        this.cellW = cellW
        this.cellH = cellH
        this.typeface = typeface
        this.baseline = baseline
    }
}

/**
 * 直近に描画したセル格子(背景+文字)を off-screen Bitmap にキャッシュする。
 *
 * `update`(UniFFI 生成の `ScreenUpdate`)は可変フィールド(`var`)を持つため Compose の
 * 安定性推論の対象にならず、このコンポーザブルは選択範囲のドラッグ操作や
 * `TerminalUiState` 内の無関係なフィールド変更(接続ステータス文言、エージェント署名
 * 確認ダイアログの表示状態等、複数の独立した UI 状態が1つの `StateFlow` にまとまって
 * いるため起きる)のたびに再実行される。このキャッシュは `update` の参照・セル寸法・
 * テーマ背景色・フォント種別が前回描画時と変わっていなければセル全走査(`drawRect`/
 * `drawText`)をスキップし、キャッシュ済み Bitmap を `drawImage` で貼り直すだけにする。
 *
 * 選択ハイライトとカーソルはこの Bitmap キャッシュの対象外とし、毎フレーム軽量に
 * 描画し直す(選択ハイライトは choice のあった行だけ、カーソルは1セル分の矩形のみ)。
 * こうすることで、カーソル色だけがテーマ間で異なる(背景色は同じ)ケースでもキャッシュキー
 * にカーソル色を含める必要がなくなる。
 */
internal class GridRenderCache {
    var bitmap: Bitmap? = null
    private var renderedUpdate: ScreenUpdate? = null
    private var renderedCellW = -1f
    private var renderedCellH = -1f
    private var renderedThemeBg = 0
    private var renderedTypeface: Typeface? = null
    private var renderedBlinkPhase = false

    /**
     * グリッド全走査(背景+文字)の再描画が必要かどうか。
     *
     * [blinkPhase] は SGR 5(blink)セルの点滅位相(タスク#22)。`update` 自体は
     * Rust 側が「blink属性が立っているかどうか」を変えない限り同一インスタンスの
     * ままなので、位相の反転(表示⇔非表示の切り替え)だけでは他のキーが変化せず
     * Bitmap キャッシュが再利用され続けてしまう(Fableレビュー2次で指摘された罠:
     * 一度描かれたきり点滅しなくなるバグ)。呼び出し側([SshTerminalCanvas])は
     * 実際にblink属性を持つセルが1つも無いときは常に同じ値(`false`)を渡すことで、
     * blinkが無い画面では位相トグルのたびに無駄な全走査が走らないようにする。
     */
    fun needsRerender(
        update: ScreenUpdate,
        cellW: Float,
        cellH: Float,
        themeBgArgb: Int,
        typeface: Typeface,
        blinkPhase: Boolean,
    ): Boolean =
        renderedUpdate !== update ||
            renderedCellW != cellW ||
            renderedCellH != cellH ||
            renderedThemeBg != themeBgArgb ||
            renderedTypeface !== typeface ||
            renderedBlinkPhase != blinkPhase

    fun markRendered(
        update: ScreenUpdate,
        cellW: Float,
        cellH: Float,
        themeBgArgb: Int,
        typeface: Typeface,
        blinkPhase: Boolean,
    ) {
        renderedUpdate = update
        renderedCellW = cellW
        renderedCellH = cellH
        renderedThemeBg = themeBgArgb
        renderedTypeface = typeface
        renderedBlinkPhase = blinkPhase
    }

    /** 次回の [needsRerender] を強制的に true にする(Bitmap を再確保したときに使う)。 */
    fun invalidate() {
        renderedUpdate = null
    }
}

/**
 * Rust側`ImagePlacement.rgba`(RGBA8888、row-major)のバイト列を、Android
 * `Bitmap.setPixels`が要求するパックド`Int`(`0xAARRGGBB`、Android公式契約)の
 * 配列へ変換する。`copyPixelsFromBuffer`のような生バイトコピーとは違い、
 * チャンネル順を明示的に組み立てるため内部バイトレイアウトに依存しない。
 */
internal fun rgbaBytesToArgbInts(rgba: ByteArray): IntArray {
    val pixels = IntArray(rgba.size / 4)
    for (i in pixels.indices) {
        val o = i * 4
        val r = rgba[o].toInt() and 0xFF
        val g = rgba[o + 1].toInt() and 0xFF
        val b = rgba[o + 2].toInt() and 0xFF
        val a = rgba[o + 3].toInt() and 0xFF
        pixels[i] = (a shl 24) or (r shl 16) or (g shl 8) or b
    }
    return pixels
}

/**
 * Sixel(タスク#42)の`ImagePlacement.rgba`から作った`Bitmap`をid単位でキャッシュする。
 * `ScreenUpdate.images`はTerminal(rust-core)側で寿命管理された「現在アクティブな
 * 画像の全リスト」がそのまま渡ってくる(rust-ssot: どの画像がまだ生きているかの
 * 判断はRust側で完結している)ため、この層は判断ロジックを持たず「今回のリストに
 * 無いidのBitmapを捨て、まだキャッシュに無いidだけ新規デコードする」宣言的な
 * 反映のみを行う。
 */
internal class SixelBitmapCache {
    private val cache = mutableMapOf<ULong, Bitmap>()

    /**
     * Rust側`sixel.rs`の`MAX_SIXEL_DIM`/`MAX_SIXEL_AREA`と同じ上限をここでも二重に
     * 適用する。通常経路ではRust側で既に弾かれているはずだが、将来別の画像プロトコル
     * (#53等)が同じ`ImagePlacement`を再利用した場合や、寸法とバッファ長が矛盾する
     * 壊れたデータが来た場合に、巨大`Bitmap`確保やクラッシュへ直結させないための
     * 防御(codexレビュー指摘)。
     */
    private fun isSane(img: ImagePlacement, w: Int, h: Int): Boolean {
        if (w <= 0 || h <= 0) return false
        if (w > 4096 || h > 4096) return false
        if (w.toLong() * h.toLong() > 4_000_000L) return false
        return img.rgba.size.toLong() == w.toLong() * h.toLong() * 4L
    }

    fun bitmapsFor(images: List<ImagePlacement>): Map<ULong, Bitmap> {
        // 画像が0件になった場合(Rust側でclear_images()された等)も必ず呼ばれる想定。
        // liveIdsが空集合になり、retainAllで古いBitmapが全て解放される(codexレビュー指摘:
        // 呼び出し側がisNotEmpty()で早期returnしていると、寿命が尽きた画像のBitmapが
        // 解放されずに残り続けてしまう)。
        val liveIds = images.map { it.id }.toSet()
        cache.keys.retainAll(liveIds)
        for (img in images) {
            if (img.id !in cache) {
                val w = img.widthPx.toInt()
                val h = img.heightPx.toInt()
                if (!isSane(img, w, h)) continue
                // `img.rgba`はRust側(lib.rs `ImagePlacement.rgba`)がRGBA8888バイト順で
                // 詰めたバッファ。`Bitmap.copyPixelsFromBuffer`はバイト順の変換を一切せず
                // 生コピーするだけなので、`Config.ARGB_8888`の内部バイトレイアウトが実機・
                // OSバージョンによってRGBA順と一致しない場合(あるいはRobolectric等の
                // テスト環境)、赤/青チャンネルが入れ替わって描画されてしまう
                // (codex/Fableレビュー指摘)。`setPixels(IntArray, ...)`はAndroidが公式に
                // 契約している`0xAARRGGBB`のパックド`Int`表現を受け取るAPIなので、
                // 内部バイトレイアウトに依存せず正しい色で描画できる。
                val bmp = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888)
                bmp.setPixels(rgbaBytesToArgbInts(img.rgba), 0, w, 0, 0, w, h)
                cache[img.id] = bmp
            }
        }
        return cache
    }
}

@Composable
fun SshTerminalCanvas(
    update: ScreenUpdate,
    selection: SelectionRange? = null,
    // タスク#66: 検索バーで現在選択中のマッチ位置(`SessionCore::search_scrollback`、
    // #37が返した`ScrollbackSearchMatch`をそのまま受け取るだけ——マッチ計算自体は
    // 一切行わない、rust-ssot)。呼び出し元([TerminalScreen.kt])が`scrollOffset`と
    // マッチの`row`が一致している間だけ非nullを渡す設計(iOS版
    // `TerminalScreenRepresentable.searchHighlight`と対称)。
    searchHighlight: ScrollbackSearchMatch? = null,
    theme: TerminalTheme = TerminalThemes.DEFAULT_DARK,
    // カスタムフォント([TerminalFontSettings])。未指定時は既定の [Typeface.MONOSPACE]。
    typeface: Typeface = Typeface.MONOSPACE,
    modifier: Modifier = Modifier,
) {
    val textPaint = remember(typeface) {
        Paint().apply {
            isAntiAlias = true
            this.typeface = typeface
        }
    }
    // SGR 3(italic)用のフォントバリアント。ボールドは既存の isFakeBoldText を
    // 引き続き使う(実 Typeface を4種類持つより単純で、iOS版のような
    // BOLD_ITALIC バリアント確保が不要)。typeface が変わったときだけ作り直す。
    val italicTypeface = remember(typeface) { Typeface.create(typeface, Typeface.ITALIC) }
    val bgPaint = remember { Paint() }
    val cursorPaint = remember { Paint() }
    val selectionPaint = remember {
        Paint().apply { color = android.graphics.Color.argb(120, 255, 255, 255) }
    }
    // タスク#66: 検索マッチのハイライト(黄系、選択範囲の白系と混同しないよう別色にする。
    // iOS版`TerminalScreenView.swift`の`UIColor.systemYellow.withAlphaComponent(0.55)`と対称)。
    val searchHighlightPaint = remember {
        Paint().apply { color = android.graphics.Color.argb(140, 255, 213, 0) }
    }
    val fontFit = remember { FontFitCache() }
    val gridCache = remember { GridRenderCache() }
    val sixelCache = remember { SixelBitmapCache() }

    // blink属性(SGR 5)を持つセルが1つも無ければタイマー自体を回さない(codexレビュー
    // 指摘: 画面にblinkが無くても永続的に再コンポーズ/全セル走査が走っていた)。
    // `update`の参照が変わったときだけ再計算すればよいので、Canvas描画スコープの
    // 外側でremember(update)しておく(iOS版`TerminalScreenView.swift`の
    // `lastDisplayHasBlink`と対称)。
    val hasBlink = remember(update) { update.cells.any { it.blink } }

    // タスク#33: 点滅カーソル(DECSCUSRの偶数パラメータ、あるいはDECSET `?12`)を
    // 実際に点滅させる必要があるかどうか。「点滅させるべきかどうか」自体は
    // `update.cursorBlink`(Rust側`Terminal`が決定した真値、rust-ssot)をそのまま
    // 見るだけ——ここではその値と`cursorVisible`/画面範囲内かどうかを組み合わせて
    // 「点滅タイマーを回す必要があるか」というUI表示専用の判断のみ行う
    // (iOS版`TerminalScreenView.swift`の`lastDisplayCursorBlinks`/`cursorInBounds`と対称)。
    val cursorBlinks = remember(update) {
        update.cursorVisible && update.cursorBlink &&
            update.cursorRow < update.rows && update.cursorCol < update.cols
    }

    // SGR 5(blink)の点滅位相。UI表示にのみ閉じたアニメーション状態であり
    // rust-ssot の対象外(「blink属性が立っているかどうか」自体は`CellData.blink`
    // としてRustが決定した値をそのまま見るだけ、iOS版`TerminalScreenView.swift`の
    // `blinkPhaseVisible`と対称)。xterm既定に近い0.53秒間隔でトグルする。
    // 点滅カーソルもこの同じ位相を共有する(xtermも同じ位相を共有する、iOS版と対称)。
    var blinkPhaseVisible by remember { mutableStateOf(true) }
    LaunchedEffect(hasBlink, cursorBlinks) {
        if (!hasBlink && !cursorBlinks) return@LaunchedEffect
        while (true) {
            delay(530)
            blinkPhaseVisible = !blinkPhaseVisible
        }
    }

    Canvas(modifier = modifier.background(theme.background)) {
        val cols = update.cols.toInt()
        val rows = update.rows.toInt()
        if (cols <= 0 || rows <= 0) return@Canvas

        val cellW = size.width / cols
        val cellH = size.height / rows
        val themeBgArgb = theme.background.toArgb()

        // blink属性(SGR 5)を持つセルが1つも無ければ、位相トグルのたびに
        // Bitmap キャッシュキーを変えて無駄な全走査を発生させない
        // (Fableレビュー2次: blink位相をキャッシュキーに含める対応。ただし
        // blinkが無い画面ではキーを固定値のままにしてキャッシュヒットを保つ)。
        // hasBlink自体はCanvas描画スコープの外(composable本体)で計算済み。
        val effectiveBlinkPhase = hasBlink && blinkPhaseVisible

        // フォントサイズをセル幅に収まるよう実測で調整
        // まず cellH ベースで設定し、M の実測幅が cellW を超えたら縮小
        // (cellW/cellH/typeface が変わらない限りキャッシュを使い回す)
        if (fontFit.needsRefit(cellW, cellH, typeface)) {
            textPaint.textSize = cellH * 0.75f
            val mWidth = textPaint.measureText("M")
            if (mWidth > cellW) {
                textPaint.textSize *= cellW / mWidth
            }
            val fm = textPaint.fontMetrics
            fontFit.markFit(cellW, cellH, typeface, baseline = -fm.top)
        }
        val baseline = fontFit.baseline

        // グリッド全体を描画する off-screen Bitmap。Canvas の実ピクセルサイズに合わせて
        // 確保し直す(回転・分割ペイン等でのリサイズ時のみ再確保が走る)。
        val pixelW = ceil(size.width).toInt().coerceAtLeast(1)
        val pixelH = ceil(size.height).toInt().coerceAtLeast(1)
        var bmp = gridCache.bitmap
        if (bmp == null || bmp.width != pixelW || bmp.height != pixelH) {
            bmp = Bitmap.createBitmap(pixelW, pixelH, Bitmap.Config.ARGB_8888)
            gridCache.bitmap = bmp
            gridCache.invalidate()
        }

        if (gridCache.needsRerender(update, cellW, cellH, themeBgArgb, typeface, effectiveBlinkPhase)) {
            // 前回描画分をクリア(既定背景色以外を描いたセルが今回は既定背景に戻る
            // ケースがあるため、単純な上書きでは古いピクセルが残ってしまう)
            bmp.eraseColor(android.graphics.Color.TRANSPARENT)
            val bitmapCanvas = android.graphics.Canvas(bmp)

            for (row in 0 until rows) {
                val y = row * cellH
                val rowStart = row * cols

                // 背景(テーマの既定背景以外が連続する区間だけをバッチ描画)
                for (run in computeBgRuns(update.cells, rowStart, cols, themeBgArgb)) {
                    bgPaint.color = run.argb
                    bitmapCanvas.drawRect(run.startCol * cellW, y, run.endColExclusive * cellW, y + cellH, bgPaint)
                }

                // 文字
                for (col in 0 until cols) {
                    val cell = update.cells[rowStart + col]
                    // invisible(SGR 8)はグリフを描かない。blink(SGR 5)は点滅位相が
                    // 「消灯」側の間だけ同様にグリフを省く(背景は通常通り描く)。
                    // reverse(SGR 7)はterminal.rs側でパース時にfg/bgへ実効色として
                    // 解決済み(#21)なので、ここではcell.fg/bg をそのまま使うだけでよい。
                    // 空白文字自体は本来 drawText 不要だが、underline/strikethrough
                    // (SGR 4/9)が立っている空白セルは装飾線だけ描く必要があるため
                    // isNotBlank() の早期スキップから除外する(codexレビュー指摘:
                    // 装飾のみの空白セルが描かれないと下線/取り消し線が消えてしまう)。
                    val blinkHidden = cell.blink && !effectiveBlinkPhase
                    val hasLineDecoration = cell.underline || cell.strikethrough
                    if (cell.ch.isNotEmpty() && (cell.ch.isNotBlank() || hasLineDecoration) &&
                        !cell.invisible && !blinkHidden
                    ) {
                        val x = col * cellW
                        val fgArgb = cell.fg.toInt()
                        textPaint.color = if (cell.dim) dimmedArgb(fgArgb) else fgArgb
                        textPaint.isFakeBoldText = cell.bold
                        textPaint.typeface = if (cell.italic) italicTypeface else typeface
                        textPaint.isUnderlineText = cell.underline
                        textPaint.isStrikeThruText = cell.strikethrough
                        bitmapCanvas.drawText(cell.ch, x, y + baseline, textPaint)
                    }
                }
            }

            // 次回の描画で typeface/isUnderlineText 等のPaint状態を汚さないよう
            // 既定値へ戻す(このPaintはグリッド以外(カーソル等)では使わないが、
            // remember で使い回されるインスタンスなので明示的にリセットしておく)。
            textPaint.typeface = typeface
            textPaint.isUnderlineText = false
            textPaint.isStrikeThruText = false

            gridCache.markRendered(update, cellW, cellH, themeBgArgb, typeface, effectiveBlinkPhase)
        }

        drawImage(
            image = bmp.asImageBitmap(),
            dstOffset = IntOffset.Zero,
            dstSize = IntSize(pixelW, pixelH),
        )

        // Sixel画像(タスク#42)。テキストグリッドの上・カーソル/選択ハイライトの下に
        // 重ねる(実端末でも画像の上にカーソルが乗ることがあるのと同じ描画順)。
        // 配置(row/col/rows_span/cols_span)の判断は一切ここでは行わず、Rust側が
        // 決めた矩形へ`rgba`を引き伸ばして描くだけ(rust-ssot)。
        // update.imagesが空でも必ずbitmapsForを呼ぶ(寿命が尽きた画像のBitmapを
        // キャッシュから解放するため。isNotEmpty()で早期returnしていた旧実装は
        // 画像が0件になった後もBitmapを保持し続けるリークがあった。codexレビュー指摘)。
        run {
            val bitmaps = sixelCache.bitmapsFor(update.images)
            for (placement in update.images) {
                val src = bitmaps[placement.id] ?: continue
                drawImage(
                    image = src.asImageBitmap(),
                    dstOffset = IntOffset(
                        (placement.col.toInt() * cellW).toInt(),
                        (placement.row.toInt() * cellH).toInt(),
                    ),
                    dstSize = IntSize(
                        (placement.colsSpan.toInt() * cellW).toInt().coerceAtLeast(1),
                        (placement.rowsSpan.toInt() * cellH).toInt().coerceAtLeast(1),
                    ),
                )
            }
        }

        // カーソル(Bitmap キャッシュ対象外。テーマのカーソル色が変わってもキャッシュキーを
        // 増やす必要がないよう、選択ハイライトと同様に毎フレーム軽量に描画し直す)
        // DECTCEM(CSI ?25l/h)でカーソルが非表示状態のときはRust側がcursorVisible=falseを
        // 立てるので、描画自体をスキップする(rust-ssot: 可視判定はRust側で行い、Kotlin側は
        // フラグをそのまま反映するだけ)。タスク#33: DECSCUSR(`CSI Ps SP q`)が選択した
        // 形状は`update.cursorShape`(Rust側`Terminal`が真値を保持、rust-ssot)からそのまま
        // 読み、block/underline/barを描き分ける。点滅そのもの(`blinkPhaseVisible`という
        // 位相)はUIローカル状態(SGR blinkと同じタイマーを共有)だが、「点滅させるべきか
        // どうか」は`update.cursorBlink`(DECSCUSRの偶数/奇数パラメータ、DECSET `?12`の
        // どちらもRust側`Terminal`が解決済み)をそのまま見るだけで、Kotlin側では判断しない
        // (iOS版`TerminalScreenView.swift`の同名分岐と対称)。
        val cursorHidden = update.cursorBlink && !blinkPhaseVisible
        if (update.cursorVisible && !cursorHidden) {
            val cx = update.cursorCol.toInt() * cellW
            val cy = update.cursorRow.toInt() * cellH
            if (cx < size.width && cy < size.height) {
                cursorPaint.color = theme.cursor.copy(alpha = 0.7f).toArgb()
                val nCanvas = drawContext.canvas.nativeCanvas
                val rect = computeCursorRect(cx, cy, cellW, cellH, update.cursorShape)
                nCanvas.drawRect(rect.left, rect.top, rect.right, rect.bottom, cursorPaint)
            }
        }

        // 選択範囲のハイライト(行単位。Bitmap キャッシュとは独立に毎フレーム描画するので、
        // ドラッグ中でも背景・文字の再走査なしに追従できる)
        selection?.let { sel ->
            val startRow = sel.startRow.coerceIn(0, rows - 1)
            val endRow = sel.endRow.coerceIn(0, rows - 1)
            if (startRow <= endRow) {
                val nCanvas = drawContext.canvas.nativeCanvas
                for (row in startRow..endRow) {
                    val y = row * cellH
                    nCanvas.drawRect(0f, y, size.width, y + cellH, selectionPaint)
                }
            }
        }

        // タスク#66: 検索バーの現在マッチのハイライト。呼び出し元(`TerminalScreen.kt`)は
        // `scrollOffset`が`searchHighlight.row`と一致するときだけこの値を渡す(rust-ssot:
        // マッチの位置計算自体は一切ここでは行わず、Rust側が返した座標をそのまま描くだけ)。
        // 矩形計算は[computeSearchHighlightRect]にピュア関数として抽出済み。
        searchHighlight?.let { match ->
            computeSearchHighlightRect(match, rows, cols, cellW, cellH)?.let { rect ->
                val nCanvas = drawContext.canvas.nativeCanvas
                nCanvas.drawRect(rect.left, rect.top, rect.right, rect.bottom, searchHighlightPaint)
            }
        }
    }
}
