import XCTest
@testable import IsekaiTerminalCoreLogic

/// Android版`KeyStepJsonTest.kt`と同じ観点の検証。
final class KeyStepJSONTests: XCTestCase {

    // ── 往復(encode → decode) ─────────────────────────────

    func testRoundTripsCtrlChar() {
        let steps: [KeyStep] = [.ctrlChar("b")]
        XCTAssertEqual(KeyStepJSON.decode(KeyStepJSON.encode(steps)), steps)
    }

    func testRoundTripsTextIncludingADoubleQuoteCharacter() {
        // tmux パックの「ペイン分割(横)」ステップ相当。JSON エスケープでハマりやすい箇所。
        let steps: [KeyStep] = [.placeholderRef("prefix"), .text("\"")]
        XCTAssertEqual(KeyStepJSON.decode(KeyStepJSON.encode(steps)), steps)
    }

    func testRoundTripsSpecial() {
        let steps: [KeyStep] = [.special(.functionKey(5))]
        XCTAssertEqual(KeyStepJSON.decode(KeyStepJSON.encode(steps)), steps)
    }

    func testRoundTripsPlaceholderRef() {
        let steps: [KeyStep] = [.placeholderRef("prefix")]
        XCTAssertEqual(KeyStepJSON.decode(KeyStepJSON.encode(steps)), steps)
    }

    func testRoundTripsFullTmuxNewWindowSequence() {
        let steps: [KeyStep] = [.placeholderRef("prefix"), .text("c")]
        XCTAssertEqual(KeyStepJSON.decode(KeyStepJSON.encode(steps)), steps)
    }

    func testRoundTripPreservesByteOutputForASequenceContainingAQuote() {
        let steps: [KeyStep] = [.ctrlChar("b"), .text("\"")]
        let before = KeySequenceCommands.toBytes(steps)
        let restored = KeyStepJSON.decode(KeyStepJSON.encode(steps))
        let after = KeySequenceCommands.toBytes(restored)
        XCTAssertEqual(before, after)
    }

    // ── 空/壊れたJSON ─────────────────────────────────────

    func testEmptyStringDecodesToEmptyList() {
        XCTAssertEqual(KeyStepJSON.decode(""), [])
    }

    func testBlankStringDecodesToEmptyList() {
        XCTAssertEqual(KeyStepJSON.decode("   "), [])
    }

    func testMalformedJsonDecodesToEmptyListInsteadOfThrowing() {
        XCTAssertEqual(KeyStepJSON.decode("{not valid json"), [])
    }

    func testNonArrayJsonDecodesToEmptyListInsteadOfThrowing() {
        XCTAssertEqual(KeyStepJSON.decode(#"{"type":"text"}"#), [])
    }

    // ── 未知 type / 復元不能な値は該当 step のみスキップ ────────

    func testUnknownTypeIsSkippedButSiblingStepsSurvive() {
        let json = #"[{"type":"ctrlChar","char":"b"},{"type":"future-unknown-type"},{"type":"text","text":"c"}]"#
        XCTAssertEqual(KeyStepJSON.decode(json), [.ctrlChar("b"), .text("c")])
    }

    func testUnknownSpecialKeyNameIsSkipped() {
        let json = #"[{"type":"special","key":"totallyUnknownKey"}]"#
        XCTAssertEqual(KeyStepJSON.decode(json), [])
    }

    func testUnsupportedFunctionKeyNumberIsSkipped() {
        let json = #"[{"type":"special","key":"functionKey","number":999}]"#
        XCTAssertEqual(KeyStepJSON.decode(json), [])
    }

    func testEmptyPlaceholderRefNameIsSkipped() {
        let json = #"[{"type":"placeholderRef","name":""}]"#
        XCTAssertEqual(KeyStepJSON.decode(json), [])
    }

    func testCtrlCharWithMultiCharacterStringIsSkipped() {
        let json = #"[{"type":"ctrlChar","char":"bb"}]"#
        XCTAssertEqual(KeyStepJSON.decode(json), [])
    }

    func testEmptyTextStepIsSkipped() {
        let json = #"[{"type":"text","text":""}]"#
        XCTAssertEqual(KeyStepJSON.decode(json), [])
    }

    func testTextFieldMissingIsSkipped() {
        let json = #"[{"type":"text"}]"#
        XCTAssertEqual(KeyStepJSON.decode(json), [])
    }
}
