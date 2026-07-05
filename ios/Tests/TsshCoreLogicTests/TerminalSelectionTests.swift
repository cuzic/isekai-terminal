import XCTest
@testable import TsshCoreLogic

/// Phase 1F-1(#48): ターミナル選択のconfig非依存な純粋ロジック(`offsetToCellPos`/
/// `reconstructSelectionText`)を検証する。Android版`TerminalSelectionTest.kt`相当。
final class TerminalSelectionTests: XCTestCase {
    private func makeCell(_ ch: Character) -> CellData {
        CellData(ch: String(ch), fg: 0xFFFFFFFF, bg: 0xFF000000, bold: false)
    }

    private func makeUpdate(rows: [String], cols: Int) -> ScreenUpdate {
        var cells: [CellData] = []
        for line in rows {
            var padded = Array(line)
            while padded.count < cols { padded.append(" ") }
            for ch in padded.prefix(cols) {
                cells.append(makeCell(ch))
            }
        }
        return ScreenUpdate(
            cols: UInt32(cols), rows: UInt32(rows.count), cells: cells,
            cursorRow: 0, cursorCol: 0, title: nil,
            applicationCursorMode: false, bracketedPasteMode: false
        )
    }

    // MARK: - offsetToCellPos

    func testOffsetToCellPosConvertsCoordinatesToCell() {
        let cell = offsetToCellPos(x: 25, y: 15, cellWidth: 10, cellHeight: 10, cols: 80, rows: 24)
        XCTAssertEqual(cell, CellPos(row: 1, col: 2))
    }

    func testOffsetToCellPosClampsOutOfBoundsCoordinates() {
        let cell = offsetToCellPos(x: -10, y: 10_000, cellWidth: 10, cellHeight: 10, cols: 80, rows: 24)
        XCTAssertEqual(cell, CellPos(row: 23, col: 0))
    }

    func testOffsetToCellPosReturnsOriginWhenDimensionsAreInvalid() {
        XCTAssertEqual(offsetToCellPos(x: 5, y: 5, cellWidth: 10, cellHeight: 10, cols: 0, rows: 0), CellPos(row: 0, col: 0))
        XCTAssertEqual(offsetToCellPos(x: 5, y: 5, cellWidth: 0, cellHeight: 10, cols: 80, rows: 24), CellPos(row: 0, col: 0))
    }

    // MARK: - reconstructSelectionText

    func testReconstructSelectionTextJoinsSingleRow() {
        let update = makeUpdate(rows: ["hello world"], cols: 20)
        let selection = SelectionRange(anchor: CellPos(row: 0, col: 0), head: CellPos(row: 0, col: 0))

        XCTAssertEqual(reconstructSelectionText(update: update, selection: selection), "hello world")
    }

    func testReconstructSelectionTextJoinsMultipleRowsWithNewline() {
        let update = makeUpdate(rows: ["line one", "line two", "line three"], cols: 20)
        let selection = SelectionRange(anchor: CellPos(row: 0, col: 0), head: CellPos(row: 2, col: 0))

        XCTAssertEqual(reconstructSelectionText(update: update, selection: selection), "line one\nline two\nline three")
    }

    func testReconstructSelectionTextUsesMinMaxRowRegardlessOfAnchorHeadOrder() {
        let update = makeUpdate(rows: ["first", "second", "third"], cols: 10)
        // anchorがheadより下(逆方向ドラッグ)でもstartRow/endRowで正しく解決される。
        let selection = SelectionRange(anchor: CellPos(row: 2, col: 0), head: CellPos(row: 0, col: 0))

        XCTAssertEqual(reconstructSelectionText(update: update, selection: selection), "first\nsecond\nthird")
    }

    func testReconstructSelectionTextTrimsTrailingWhitespacePerLine() {
        let update = makeUpdate(rows: ["padded  "], cols: 20)
        let selection = SelectionRange(anchor: CellPos(row: 0, col: 0), head: CellPos(row: 0, col: 0))

        XCTAssertEqual(reconstructSelectionText(update: update, selection: selection), "padded")
    }

    func testReconstructSelectionTextReturnsEmptyForInvalidDimensions() {
        let update = ScreenUpdate(cols: 0, rows: 0, cells: [], cursorRow: 0, cursorCol: 0, title: nil, applicationCursorMode: false, bracketedPasteMode: false)
        let selection = SelectionRange(anchor: CellPos(row: 0, col: 0), head: CellPos(row: 0, col: 0))

        XCTAssertEqual(reconstructSelectionText(update: update, selection: selection), "")
    }

    func testReconstructSelectionTextClampsRowsOutOfRange() {
        let update = makeUpdate(rows: ["only line"], cols: 20)
        let selection = SelectionRange(anchor: CellPos(row: 0, col: 0), head: CellPos(row: 50, col: 0))

        XCTAssertEqual(reconstructSelectionText(update: update, selection: selection), "only line")
    }
}
