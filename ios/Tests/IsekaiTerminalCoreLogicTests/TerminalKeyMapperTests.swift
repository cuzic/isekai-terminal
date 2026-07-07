import XCTest
@testable import IsekaiTerminalCoreLogic

/// Phase 1B: TerminalKeyMapper(キー→制御シーケンス変換)の検証。
final class TerminalKeyMapperTests: XCTestCase {
    func testControlByteForLowercaseAndUppercase() {
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "c"), 0x03) // Ctrl+C
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "C"), 0x03)
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "d"), 0x04) // Ctrl+D
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "a"), 0x01) // Ctrl+A
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "z"), 0x1A) // Ctrl+Z
    }

    func testControlByteReturnsNilForDigitsAndNonAscii() {
        XCTAssertNil(TerminalKeyMapper.controlByte(for: "1"))
        XCTAssertNil(TerminalKeyMapper.controlByte(for: "あ"))
    }

    /// Rust側(`terminal_ctrl_byte`)への統合により、Android版と同じ
    /// `@ [ \ ] ^ _ ? space`もCtrl+<記号>として変換されるようになった
    /// (統合前のiOS版はアルファベットのみ対応していた)。
    func testControlByteSupportsAndroidParitySymbols() {
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "@"), 0x00)
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "["), 0x1B)
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "?"), 0x7F)
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: " "), 0x00)
    }

    func testArrowKeySequences() {
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .arrowUp), Array("\u{1B}[A".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .arrowDown), Array("\u{1B}[B".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .arrowRight), Array("\u{1B}[C".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .arrowLeft), Array("\u{1B}[D".utf8))
    }

    func testEscapeTabBackspaceDelete() {
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .escape), [0x1B])
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .tab), [0x09])
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .backspace), [0x7F])
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .delete), Array("\u{1B}[3~".utf8))
    }

    func testFunctionKeysF1ThroughF4UseSS3Form() {
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(1)), Array("\u{1B}OP".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(2)), Array("\u{1B}OQ".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(3)), Array("\u{1B}OR".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(4)), Array("\u{1B}OS".utf8))
    }

    func testFunctionKeysF5ThroughF12UseCsiTildeForm() {
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(5)), Array("\u{1B}[15~".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(12)), Array("\u{1B}[24~".utf8))
    }

    func testUnsupportedFunctionKeyReturnsEmpty() {
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(99)), [])
    }

    func testHomeEndPageUpPageDown() {
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .home), Array("\u{1B}[H".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .end), Array("\u{1B}[F".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .pageUp), Array("\u{1B}[5~".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .pageDown), Array("\u{1B}[6~".utf8))
    }
}
