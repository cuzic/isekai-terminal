package com.example.imespike.spike

import android.graphics.Paint
import android.graphics.Rect
import android.graphics.Typeface
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.example.imespike.util.RemoteLogger
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive

data class TermCell(
    val char: String = " ",
    val fg: Color = Color(0xFFCCCCCC),
    val bg: Color = Color.Transparent,
    val bold: Boolean = false,
)

data class ScreenModel(
    val cols: Int = 80,
    val rows: Int = 24,
    val cells: Array<Array<TermCell>> = Array(24) { Array(80) { TermCell() } },
    val cursorRow: Int = 0,
    val cursorCol: Int = 0,
) {
    fun withRandomUpdate(): ScreenModel {
        val newCells = Array(rows) { r -> Array(cols) { c -> cells[r][c] } }
        repeat(80) {
            val r = (Math.random() * rows).toInt()
            val c = (Math.random() * cols).toInt()
            val chars = "abcdefghijklmnopqrstuvwxyz0123456789"
            newCells[r][c] = TermCell(
                char = chars[(Math.random() * chars.length).toInt()].toString(),
                fg = Color(
                    red = (Math.random()).toFloat(),
                    green = (Math.random()).toFloat(),
                    blue = (Math.random()).toFloat(),
                ),
            )
        }
        return copy(
            cells = newCells,
            cursorRow = (Math.random() * rows).toInt(),
            cursorCol = (Math.random() * cols).toInt(),
        )
    }
}

@Composable
fun CanvasSpikeScreen() {
    var screen by remember { mutableStateOf(ScreenModel()) }
    var fps by remember { mutableStateOf(0) }
    var frameCount by remember { mutableStateOf(0) }
    var lastSecond by remember { mutableStateOf(System.currentTimeMillis()) }

    LaunchedEffect(Unit) {
        RemoteLogger.i("CanvasSpike", "render loop started cols=${screen.cols} rows=${screen.rows}")
        try {
            while (isActive) {
                screen = screen.withRandomUpdate()
                frameCount++
                val now = System.currentTimeMillis()
                if (now - lastSecond >= 1000L) {
                    fps = frameCount
                    RemoteLogger.d("CanvasSpike", "FPS=$fps")
                    frameCount = 0
                    lastSecond = now
                }
                delay(16L)
            }
        } catch (e: Exception) {
            RemoteLogger.e("CanvasSpike", "render loop exception", e)
        }
    }

    Column(modifier = Modifier.fillMaxSize().background(Color.Black)) {
        Text(
            text = "Canvas Spike — ${screen.cols}×${screen.rows}  FPS: $fps",
            color = Color.Yellow,
            fontSize = 12.sp,
            modifier = Modifier.padding(4.dp)
        )
        TerminalSurface(screen = screen, modifier = Modifier.fillMaxSize())
    }
}

@Composable
fun TerminalSurface(screen: ScreenModel, modifier: Modifier = Modifier) {
    val density = LocalDensity.current

    val cellWidthSp = 7.8f
    val cellHeightSp = 16f

    // Paint を remember でキャッシュ（毎フレーム生成しない）
    val textPaint = remember {
        Paint().apply {
            isAntiAlias = true
            typeface = Typeface.MONOSPACE
            textSize = 0f  // Canvas の onDraw で px 値をセットする
        }
    }
    val bgPaint = remember { Paint() }

    Canvas(modifier = modifier.background(Color.Black)) {
        val cellW = with(density) { cellWidthSp.sp.toPx() }
        val cellH = with(density) { cellHeightSp.sp.toPx() }

        // テキストサイズをセル高さに合わせる
        if (textPaint.textSize != cellH * 0.75f) {
            textPaint.textSize = cellH * 0.75f
        }

        // ベースライン: テキストをセル内で縦中央寄せ
        val bounds = Rect()
        textPaint.getTextBounds("M", 0, 1, bounds)
        val baseline = (cellH + bounds.height()) / 2f

        val nCanvas = drawContext.canvas.nativeCanvas

        for (row in 0 until screen.rows) {
            val y = row * cellH
            if (y >= size.height) break

            for (col in 0 until screen.cols) {
                val x = col * cellW
                if (x >= size.width) break

                val cell = screen.cells[row][col]

                // 背景
                if (cell.bg != Color.Transparent) {
                    bgPaint.color = cell.bg.toArgb()
                    nCanvas.drawRect(x, y, x + cellW, y + cellH, bgPaint)
                }

                // 文字（nativeCanvas.drawText は Paint 1 回呼ぶだけで済む）
                if (cell.char.isNotBlank()) {
                    textPaint.color = cell.fg.toArgb()
                    textPaint.isFakeBoldText = cell.bold
                    nCanvas.drawTextRun(cell.char, 0, cell.char.length, 0, cell.char.length, x, y + baseline, false, textPaint)
                }
            }
        }

        // カーソル
        drawCursor(screen.cursorRow, screen.cursorCol, cellW, cellH)
    }
}

private fun DrawScope.drawCursor(row: Int, col: Int, cellW: Float, cellH: Float) {
    drawRect(
        color = Color.White.copy(alpha = 0.7f),
        topLeft = Offset(col * cellW, row * cellH),
        size = Size(cellW, cellH),
    )
}
