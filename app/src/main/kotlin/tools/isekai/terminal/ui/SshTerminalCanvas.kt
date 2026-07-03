package tools.isekai.terminal.ui

import android.graphics.Paint
import android.graphics.Typeface
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.toArgb
import uniffi.tssh_core.ScreenUpdate

@Composable
fun SshTerminalCanvas(update: ScreenUpdate, selection: SelectionRange? = null, modifier: Modifier = Modifier) {
    val textPaint = remember {
        Paint().apply {
            isAntiAlias = true
            typeface = Typeface.MONOSPACE
        }
    }
    val bgPaint = remember { Paint() }
    val selectionPaint = remember {
        Paint().apply { color = android.graphics.Color.argb(120, 255, 255, 255) }
    }

    Canvas(modifier = modifier.background(Color.Black)) {
        val cols = update.cols.toInt()
        val rows = update.rows.toInt()

        val cellW = size.width / cols
        val cellH = size.height / rows

        // フォントサイズをセル幅に収まるよう実測で調整
        // まず cellH ベースで設定し、M の実測幅が cellW を超えたら縮小
        textPaint.textSize = cellH * 0.75f
        val mWidth = textPaint.measureText("M")
        if (mWidth > cellW) {
            textPaint.textSize *= cellW / mWidth
        }

        // ベースライン計算
        val fm = textPaint.fontMetrics
        val baseline = -fm.top

        val nCanvas = drawContext.canvas.nativeCanvas

        // 選択範囲のハイライト（行単位。文字描画より前に半透明の反転色で塗る）
        selection?.let { sel ->
            val startRow = sel.startRow.coerceIn(0, rows - 1)
            val endRow = sel.endRow.coerceIn(0, rows - 1)
            if (startRow <= endRow) {
                for (row in startRow..endRow) {
                    val y = row * cellH
                    nCanvas.drawRect(0f, y, size.width, y + cellH, selectionPaint)
                }
            }
        }

        for (row in 0 until rows) {
            val y = row * cellH
            for (col in 0 until cols) {
                val x = col * cellW
                val cell = update.cells[row * cols + col]
                val bg = cell.bg.toInt()
                val fg = cell.fg.toInt()

                // 背景（デフォルト黒以外のみ描画）
                if (bg != android.graphics.Color.BLACK) {
                    bgPaint.color = bg
                    nCanvas.drawRect(x, y, x + cellW, y + cellH, bgPaint)
                }

                // 文字
                if (cell.ch.isNotBlank()) {
                    textPaint.color = fg
                    textPaint.isFakeBoldText = cell.bold
                    nCanvas.drawText(cell.ch, x, y + baseline, textPaint)
                }
            }
        }

        // カーソル
        val cx = update.cursorCol.toInt() * cellW
        val cy = update.cursorRow.toInt() * cellH
        if (cx < size.width && cy < size.height) {
            bgPaint.color = Color.White.copy(alpha = 0.7f).toArgb()
            nCanvas.drawRect(cx, cy, cx + cellW, cy + cellH, bgPaint)
        }
    }
}
