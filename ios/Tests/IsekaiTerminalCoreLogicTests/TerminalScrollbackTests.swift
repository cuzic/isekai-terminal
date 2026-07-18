import XCTest
@testable import IsekaiTerminalCoreLogic

/// Phase 1F-4(#51): スクロールバック表示用の`ScreenUpdate`合成ロジックを検証する。
/// Android版`TerminalScreen.kt`の`displayUpdate`と対称。
final class TerminalScrollbackTests: XCTestCase {
    private func makeCell(_ ch: Character) -> CellData {
        CellData(
            ch: String(ch), fg: 0xFFFFFFFF, bg: 0xFF000000, bold: false,
            dim: false, italic: false, underline: false,
            strikethrough: false, blink: false, invisible: false
        )
    }

    private func makeUpdate(
        rows: [String], cols: Int, cursorRow: UInt32 = 0, cursorCol: UInt32 = 0,
        cursorVisible: Bool = true, bellGeneration: UInt64 = 0
    ) -> ScreenUpdate {
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
            cursorRow: cursorRow, cursorCol: cursorCol, title: "session",
            applicationCursorMode: true, bracketedPasteMode: true,
            cursorVisible: cursorVisible, bellGeneration: bellGeneration
        )
    }

    func testReturnsLiveUpdateWhenScrollOffsetIsZero() {
        let live = makeUpdate(rows: ["live line"], cols: 20, cursorRow: 0, cursorCol: 3)

        let result = synthesizeDisplayUpdate(live: live, scrollOffset: 0, scrollbackCells: [])

        XCTAssertEqual(result, live)
    }

    func testSynthesizesScrollbackUpdateWhenOffsetIsPositive() {
        let live = makeUpdate(rows: ["live line"], cols: 20, cursorRow: 0, cursorCol: 3)
        let scrollbackCells = Array(repeating: makeCell("x"), count: 20)

        let result = synthesizeDisplayUpdate(live: live, scrollOffset: 5, scrollbackCells: scrollbackCells)

        XCTAssertEqual(result.cells, scrollbackCells)
        XCTAssertEqual(result.cols, live.cols)
        XCTAssertEqual(result.rows, live.rows)
        XCTAssertEqual(result.title, live.title)
        XCTAssertEqual(result.applicationCursorMode, live.applicationCursorMode)
        XCTAssertEqual(result.bracketedPasteMode, live.bracketedPasteMode)
        XCTAssertEqual(result.cursorVisible, live.cursorVisible)
        XCTAssertEqual(result.bellGeneration, live.bellGeneration)
    }

    func testPreservesBellGenerationWhenShowingScrollback() {
        // bellGenerationはBEL通知用のRust側SSOTカウンタ(タスク#24)。スクロールバック
        // 表示に切り替えても直近のライブ値を落としてはいけない(Android版`TerminalScreen.kt`
        // の`bellGeneration = update.bellGeneration`と対称)。
        let live = makeUpdate(rows: ["live line"], cols: 20, bellGeneration: 7)
        let scrollbackCells = Array(repeating: makeCell("x"), count: 20)

        let result = synthesizeDisplayUpdate(live: live, scrollOffset: 5, scrollbackCells: scrollbackCells)

        XCTAssertEqual(result.bellGeneration, 7)
    }

    func testPreservesCursorVisibilityFalseWhenShowingScrollback() {
        let live = makeUpdate(rows: ["live line"], cols: 20, cursorVisible: false)
        let scrollbackCells = Array(repeating: makeCell("x"), count: 20)

        let result = synthesizeDisplayUpdate(live: live, scrollOffset: 5, scrollbackCells: scrollbackCells)

        XCTAssertEqual(result.cursorVisible, false)
    }

    func testHidesCursorOffScreenWhenShowingScrollback() {
        let live = makeUpdate(rows: ["live line"], cols: 20, cursorRow: 0, cursorCol: 3)
        let scrollbackCells = Array(repeating: makeCell("x"), count: 20)

        let result = synthesizeDisplayUpdate(live: live, scrollOffset: 5, scrollbackCells: scrollbackCells)

        // カーソルは画面外(rows行目、0-indexedの範囲外)に隠される。
        XCTAssertEqual(result.cursorRow, live.rows)
        XCTAssertEqual(result.cursorCol, 0)
    }

    func testFallsBackToLiveWhenScrollbackCellCountMismatches() {
        let live = makeUpdate(rows: ["live line"], cols: 20)
        let wrongSizeCells = [makeCell("x")]

        let result = synthesizeDisplayUpdate(live: live, scrollOffset: 5, scrollbackCells: wrongSizeCells)

        XCTAssertEqual(result, live)
    }
}
