import Foundation

/// ビューポート内の行・列(0-indexed)。Android版`TerminalSelection.kt`の`CellPos`と対称。
/// MVPの選択は行単位(linewise)のため`col`は将来のセル単位選択拡張用に保持するのみで、
/// 選択範囲の計算自体には使わない。
public struct CellPos: Equatable {
    public let row: Int
    public let col: Int

    public init(row: Int, col: Int) {
        self.row = row
        self.col = col
    }
}

/// テキスト選択範囲。`anchor`は長押しで選択を開始した位置、`head`はドラッグ中に更新される
/// 終端位置。MVPは行単位(linewise)選択なので、実際に選択されるのは
/// `min(anchor.row, head.row)...max(anchor.row, head.row)`の全行・全列
/// (Android版`SelectionRange`と対称)。
///
/// スクロール位置と同じく「UI表示だけに閉じた状態」(`.claude/rules/rust-ssot.md`の例外)
/// としてSwiftUI側のView状態(`@State`)で保持する。Rust側に選択状態を持たせない。
public struct SelectionRange: Equatable {
    public let anchor: CellPos
    public var head: CellPos

    public init(anchor: CellPos, head: CellPos) {
        self.anchor = anchor
        self.head = head
    }

    public var startRow: Int { min(anchor.row, head.row) }
    public var endRow: Int { max(anchor.row, head.row) }
}

/// 画面上の座標(x, y)をセル位置に変換する。ビューポート範囲外はクランプする。
/// cols/rowsが0以下(未初期化画面等)の場合は(0, 0)を返す。
///
/// 引数は`CGFloat`ではなく`Double`(呼び出し元のUIKit層で`Double(point.x)`のように
/// 変換する)。`CoreGraphics`はLinuxに存在せず、この関数をLinuxでも`swift test`
/// 可能にする(`IsekaiTerminalCoreLogic`ターゲット)ためにプラットフォーム非依存の型にしている。
public func offsetToCellPos(x: Double, y: Double, cellWidth: Double, cellHeight: Double, cols: Int, rows: Int) -> CellPos {
    guard cols > 0, rows > 0, cellWidth > 0, cellHeight > 0 else { return CellPos(row: 0, col: 0) }
    let col = min(max(Int(x / cellWidth), 0), cols - 1)
    let row = min(max(Int(y / cellHeight), 0), rows - 1)
    return CellPos(row: row, col: col)
}

/// 選択範囲を`update`のcells(行優先のフラット配列)からコピー用テキストへ再構成する。
///
/// 各行はcell.chを連結し、行末の空白セルのみtrimする(行中の空白セルはtrimしない —
/// `CellData`には全角文字の継続セルを区別するフラグがないため、これは既知のMVP制約、
/// Android版`reconstructSelectionText`と同じ)。複数行は"\n"で結合する。
/// 範囲外・空データの場合は空文字列を返す。
public func reconstructSelectionText(update: ScreenUpdate, selection: SelectionRange) -> String {
    let cols = Int(update.cols)
    let rows = Int(update.rows)
    guard cols > 0, rows > 0, update.cells.count >= rows * cols else { return "" }

    let startRow = min(max(selection.startRow, 0), rows - 1)
    let endRow = min(max(selection.endRow, 0), rows - 1)
    guard startRow <= endRow else { return "" }

    var lines: [String] = []
    for row in startRow...endRow {
        let base = row * cols
        var line = ""
        for col in 0..<cols {
            line += update.cells[base + col].ch
        }
        while let last = line.last, last.isWhitespace {
            line.removeLast()
        }
        lines.append(line)
    }
    return lines.joined(separator: "\n")
}
