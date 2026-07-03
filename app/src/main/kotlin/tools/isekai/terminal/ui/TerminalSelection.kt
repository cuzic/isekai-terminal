package tools.isekai.terminal.ui

import uniffi.tssh_core.ScreenUpdate

/**
 * ビューポート内の行・列（0-indexed）。
 * MVP の選択は行単位(linewise)のため col は将来のセル単位選択拡張用に保持するのみで、
 * 選択範囲の計算自体には使わない。
 */
data class CellPos(val row: Int, val col: Int)

/**
 * テキスト選択範囲。[anchor] は長押しで選択を開始した位置、[head] はドラッグ中に更新される
 * 終端位置。MVP は行単位(linewise)選択なので、実際に選択されるのは
 * `min(anchor.row, head.row)..max(anchor.row, head.row)` の全行・全列。
 *
 * スクロール位置と同じく「UI 表示だけに閉じた状態」（.claude/rules/rust-ssot.md の例外）
 * として Compose local state で保持する。Rust 側に選択状態を持たせない。
 */
data class SelectionRange(val anchor: CellPos, val head: CellPos) {
    val startRow: Int get() = minOf(anchor.row, head.row)
    val endRow: Int get() = maxOf(anchor.row, head.row)
}

/**
 * 画面上の座標 (x, y) をセル位置に変換する。ビューポート範囲外はクランプする。
 * cols/rows が 0 以下（未初期化画面など）の場合は (0, 0) を返す。
 */
fun offsetToCellPos(x: Float, y: Float, cellWidth: Float, cellHeight: Float, cols: Int, rows: Int): CellPos {
    if (cols <= 0 || rows <= 0 || cellWidth <= 0f || cellHeight <= 0f) return CellPos(0, 0)
    val col = (x / cellWidth).toInt().coerceIn(0, cols - 1)
    val row = (y / cellHeight).toInt().coerceIn(0, rows - 1)
    return CellPos(row, col)
}

/**
 * 選択範囲を [update] の cells（行優先のフラット配列、`TerminalScreen.kt` の displayUpdate 参照）
 * からコピー用テキストへ再構成する。
 *
 * 各行は cell.ch を連結し、行末の空白セルのみ trim する（行中の空白セルは trim しない —
 * CellData には全角文字の継続セルを区別するフラグがないため、これは既知の MVP 制約）。
 * 複数行は "\n" で結合する。範囲外・空データの場合は空文字列を返す。
 */
fun reconstructSelectionText(update: ScreenUpdate, selection: SelectionRange): String {
    val cols = update.cols.toInt()
    val rows = update.rows.toInt()
    if (cols <= 0 || rows <= 0) return ""
    val cells = update.cells
    if (cells.size < rows * cols) return ""

    val startRow = selection.startRow.coerceIn(0, rows - 1)
    val endRow = selection.endRow.coerceIn(0, rows - 1)
    if (startRow > endRow) return ""

    return (startRow..endRow).joinToString("\n") { row ->
        val base = row * cols
        val sb = StringBuilder(cols)
        for (col in 0 until cols) sb.append(cells[base + col].ch)
        sb.toString().trimEnd()
    }
}
