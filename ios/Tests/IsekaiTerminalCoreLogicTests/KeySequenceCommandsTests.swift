import XCTest
@testable import IsekaiTerminalCoreLogic

/// Android版`KeySequenceCommandsTest.kt`と同じ観点の検証。
final class KeySequenceCommandsTests: XCTestCase {

    // ── 各ステップ種別が既存の型/Rust委譲関数へ委譲されること ─────────

    func testCtrlCharDelegatesToTerminalKeyMapperControlByte() {
        let bytes = KeySequenceCommands.toBytes([.ctrlChar("b")])
        XCTAssertEqual(bytes, Data([0x02])) // Ctrl+B
    }

    func testSpecialDelegatesToTerminalKeyMapperBytes() {
        let bytes = KeySequenceCommands.toBytes([.special(.escape)])
        XCTAssertEqual(bytes, Data([0x1B]))
    }

    func testTextDelegatesToTerminalCommitTextBytesWithoutForcingTrailingCR() {
        // KeySequenceCommands は SnippetCommands と違い、単発キー入力に余計な CR を付与しない。
        let bytes = KeySequenceCommands.toBytes([.text("c")])
        XCTAssertEqual(bytes, Data("c".utf8))
    }

    func testPlaceholderRefProducesNoBytes() {
        let bytes = KeySequenceCommands.toBytes([.placeholderRef("prefix")])
        XCTAssertEqual(bytes, Data())
    }

    // ── 委譲元が変換不能な場合はスキップされる ─────────────────────

    func testInvalidCtrlCharProducesNoBytes() {
        let bytes = KeySequenceCommands.toBytes([.ctrlChar("1")])
        XCTAssertEqual(bytes, Data())
    }

    // ── 組み立て(複数ステップの連結) ──────────────────────────

    func testEmptyStepsProducesEmptyBytes() {
        XCTAssertEqual(KeySequenceCommands.toBytes([]), Data())
    }

    func testTmuxNewWindowSequenceConcatenatesPrefixChordAndLiteralC() {
        // {prefix}=Ctrl+B, 'c' の想定(実際のパック解決はTask #23側の責務)。
        let bytes = KeySequenceCommands.toBytes([.ctrlChar("b"), .text("c")])
        XCTAssertEqual(bytes, Data([0x02, UInt8(ascii: "c")]))
    }

    func testLargeTextStepIsPassedThroughUnmodifiedAsideFromNewlineNormalization() {
        let big = String(repeating: "x", count: 10_000)
        let bytes = KeySequenceCommands.toBytes([.text(big)])
        XCTAssertEqual(bytes, Data(big.utf8))
    }

    // ── applicationCursorMode の伝播 ──────────────────────────

    func testArrowKeyWithoutApplicationCursorModeUsesCsiForm() {
        let bytes = KeySequenceCommands.toBytes([.special(.arrowUp)], applicationCursorMode: false)
        XCTAssertEqual(bytes, Data([0x1B, 0x5B, 0x41]))
    }

    func testArrowKeyWithApplicationCursorModeUsesSs3Form() {
        let bytes = KeySequenceCommands.toBytes([.special(.arrowUp)], applicationCursorMode: true)
        XCTAssertEqual(bytes, Data([0x1B, 0x4F, 0x41]))
    }
}
