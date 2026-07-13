package tools.isekai.terminal.ui

import android.graphics.Bitmap
import android.graphics.Paint
import android.graphics.Typeface
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import kotlin.math.ceil
import uniffi.isekai_terminal_core.CellData
import uniffi.isekai_terminal_core.ScreenUpdate

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

    /** グリッド全走査(背景+文字)の再描画が必要かどうか。 */
    fun needsRerender(update: ScreenUpdate, cellW: Float, cellH: Float, themeBgArgb: Int, typeface: Typeface): Boolean =
        renderedUpdate !== update ||
            renderedCellW != cellW ||
            renderedCellH != cellH ||
            renderedThemeBg != themeBgArgb ||
            renderedTypeface !== typeface

    fun markRendered(update: ScreenUpdate, cellW: Float, cellH: Float, themeBgArgb: Int, typeface: Typeface) {
        renderedUpdate = update
        renderedCellW = cellW
        renderedCellH = cellH
        renderedThemeBg = themeBgArgb
        renderedTypeface = typeface
    }

    /** 次回の [needsRerender] を強制的に true にする(Bitmap を再確保したときに使う)。 */
    fun invalidate() {
        renderedUpdate = null
    }
}

@Composable
fun SshTerminalCanvas(
    update: ScreenUpdate,
    selection: SelectionRange? = null,
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
    val bgPaint = remember { Paint() }
    val cursorPaint = remember { Paint() }
    val selectionPaint = remember {
        Paint().apply { color = android.graphics.Color.argb(120, 255, 255, 255) }
    }
    val fontFit = remember { FontFitCache() }
    val gridCache = remember { GridRenderCache() }

    Canvas(modifier = modifier.background(theme.background)) {
        val cols = update.cols.toInt()
        val rows = update.rows.toInt()
        if (cols <= 0 || rows <= 0) return@Canvas

        val cellW = size.width / cols
        val cellH = size.height / rows
        val themeBgArgb = theme.background.toArgb()

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

        if (gridCache.needsRerender(update, cellW, cellH, themeBgArgb, typeface)) {
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
                    if (cell.ch.isNotBlank()) {
                        val x = col * cellW
                        textPaint.color = cell.fg.toInt()
                        textPaint.isFakeBoldText = cell.bold
                        bitmapCanvas.drawText(cell.ch, x, y + baseline, textPaint)
                    }
                }
            }

            gridCache.markRendered(update, cellW, cellH, themeBgArgb, typeface)
        }

        drawImage(
            image = bmp.asImageBitmap(),
            dstOffset = IntOffset.Zero,
            dstSize = IntSize(pixelW, pixelH),
        )

        // カーソル(Bitmap キャッシュ対象外。テーマのカーソル色が変わってもキャッシュキーを
        // 増やす必要がないよう、選択ハイライトと同様に毎フレーム軽量に描画し直す)
        val cx = update.cursorCol.toInt() * cellW
        val cy = update.cursorRow.toInt() * cellH
        if (cx < size.width && cy < size.height) {
            cursorPaint.color = theme.cursor.copy(alpha = 0.7f).toArgb()
            val nCanvas = drawContext.canvas.nativeCanvas
            nCanvas.drawRect(cx, cy, cx + cellW, cy + cellH, cursorPaint)
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
    }
}
