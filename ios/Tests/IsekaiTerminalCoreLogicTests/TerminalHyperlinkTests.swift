import XCTest
@testable import IsekaiTerminalCoreLogic

/// OSC 8ハイパーリンク(タスク#40)のタップUI(タスク#52)向け純粋ロジックを検証する。
/// Android版`TerminalHyperlinkTest.kt`相当。
final class TerminalHyperlinkTests: XCTestCase {
    private func makeCell(_ ch: Character, linkId: UInt32?) -> CellData {
        CellData(
            ch: String(ch), fg: 0xFFFFFFFF, bg: 0xFF000000, bold: false,
            dim: false, italic: false, underline: false,
            strikethrough: false, blink: false, invisible: false, linkId: linkId
        )
    }

    private func makeUpdate(cells: [CellData], cols: Int, rows: Int, linkTable: [String]) -> ScreenUpdate {
        ScreenUpdate(
            updateSeq: 0, cols: UInt32(cols), rows: UInt32(rows), cells: cells,
            cursorRow: 0, cursorCol: 0, title: nil,
            applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
            mouseReportingMode: .off, sgrMouseMode: false, alternateScroll: false, urxvtMouseMode: false,
            cursorVisible: true, bellGeneration: 0,
            cursorShape: .block, cursorBlink: true, linkTable: linkTable, images: [], kittyKeyboardFlags: 0, dirtyRows: nil
        )
    }

    // MARK: - linkURL(at:row:col:)

    func testLinkURLResolvesLinkIdThroughLinkTable() {
        let cells = [makeCell("h", linkId: 0), makeCell("i", linkId: 0), makeCell(" ", linkId: nil)]
        let update = makeUpdate(cells: cells, cols: 3, rows: 1, linkTable: ["https://example.com"])

        XCTAssertEqual(linkURL(at: update, row: 0, col: 0), "https://example.com")
        XCTAssertEqual(linkURL(at: update, row: 0, col: 1), "https://example.com")
    }

    func testLinkURLReturnsNilWhenCellHasNoLink() {
        let cells = [makeCell("h", linkId: 0), makeCell(" ", linkId: nil)]
        let update = makeUpdate(cells: cells, cols: 2, rows: 1, linkTable: ["https://example.com"])

        XCTAssertNil(linkURL(at: update, row: 0, col: 1))
    }

    func testLinkURLReturnsNilForOutOfBoundsCoordinates() {
        let update = makeUpdate(cells: [makeCell("h", linkId: 0)], cols: 1, rows: 1, linkTable: ["https://example.com"])

        XCTAssertNil(linkURL(at: update, row: -1, col: 0))
        XCTAssertNil(linkURL(at: update, row: 0, col: 5))
        XCTAssertNil(linkURL(at: update, row: 5, col: 0))
    }

    func testLinkURLReturnsNilWhenDimensionsAreDegenerate() {
        let update = makeUpdate(cells: [], cols: 0, rows: 0, linkTable: [])
        XCTAssertNil(linkURL(at: update, row: 0, col: 0))
    }

    func testLinkURLReturnsNilWhenLinkIdIsOutOfLinkTableBounds() {
        // 本来Rust側は常に有効なindexしかセルへ書かないはずだが、呼び出し側の
        // 防御としてクラッシュせずnilを返すことを確認する。
        let update = makeUpdate(cells: [makeCell("h", linkId: 99)], cols: 1, rows: 1, linkTable: ["https://example.com"])
        XCTAssertNil(linkURL(at: update, row: 0, col: 0))
    }

    // MARK: - isOpenableHyperlinkScheme

    func testHttpAndHttpsSchemesAreOpenable() {
        XCTAssertTrue(isOpenableHyperlinkScheme("http://example.com"))
        XCTAssertTrue(isOpenableHyperlinkScheme("https://example.com/path?x=1"))
        // スキームは大文字小文字を区別しない(RFC 3986)。
        XCTAssertTrue(isOpenableHyperlinkScheme("HTTPS://EXAMPLE.COM"))
    }

    func testDangerousSchemesAreRejected() {
        XCTAssertFalse(isOpenableHyperlinkScheme("intent://example.com#Intent;scheme=https;end"))
        XCTAssertFalse(isOpenableHyperlinkScheme("file:///etc/passwd"))
        XCTAssertFalse(isOpenableHyperlinkScheme("javascript:alert(1)"))
        XCTAssertFalse(isOpenableHyperlinkScheme("tel:+1234567890"))
    }

    func testStringsWithoutAValidSchemeAreRejected() {
        XCTAssertFalse(isOpenableHyperlinkScheme(""))
        XCTAssertFalse(isOpenableHyperlinkScheme("example.com"))
        XCTAssertFalse(isOpenableHyperlinkScheme("not a url at all"))
    }
}
