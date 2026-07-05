import XCTest
@testable import TsshCore

/// Phase 1G-1(#53): `SnippetCommands.toBytes`の検証。Android版`SnippetCommands.kt`の
/// テストと同じケースを移植した。
final class SnippetCommandsTests: XCTestCase {
    func testEmptyCommandReturnsEmptyBytes() {
        XCTAssertEqual(SnippetCommands.toBytes(command: ""), Data())
    }

    func testSingleLineAppendsTrailingCR() {
        let bytes = SnippetCommands.toBytes(command: "ls -la")
        XCTAssertEqual(String(data: bytes, encoding: .utf8), "ls -la\r")
    }

    func testAppendNewlineFalseLeavesLastLineWithoutCR() {
        let bytes = SnippetCommands.toBytes(command: "ls -la", appendNewline: false)
        XCTAssertEqual(String(data: bytes, encoding: .utf8), "ls -la")
    }

    func testNewlinesAreNormalizedToCR() {
        let bytes = SnippetCommands.toBytes(command: "echo one\necho two", appendNewline: false)
        XCTAssertEqual(String(data: bytes, encoding: .utf8), "echo one\recho two")
    }

    func testCRLFIsNormalizedToSingleCR() {
        let bytes = SnippetCommands.toBytes(command: "echo one\r\necho two", appendNewline: false)
        XCTAssertEqual(String(data: bytes, encoding: .utf8), "echo one\recho two")
    }

    func testDoesNotDoubleTrailingCRWhenAlreadyPresent() {
        let bytes = SnippetCommands.toBytes(command: "ls -la\n", appendNewline: true)
        XCTAssertEqual(String(data: bytes, encoding: .utf8), "ls -la\r")
    }

    func testSnippetOverloadUsesSnippetFields() {
        let snippet = Snippet(label: "test", command: "echo hi", appendNewline: false)
        let bytes = SnippetCommands.toBytes(snippet: snippet)
        XCTAssertEqual(String(data: bytes, encoding: .utf8), "echo hi")
    }
}
