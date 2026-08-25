import XCTest
@testable import IsekaiTerminalCoreLogic

/// Y-P1(#2): `AiPanelFormSubmission.bytes`の検証(`ADR_IOS_PARITY_IMPLEMENTATION.md` §3.2)。
final class AiPanelFormSubmissionTests: XCTestCase {
    private func string(_ data: Data) -> String {
        String(data: data, encoding: .utf8)!
    }

    func testEmptyFormProducesEmptyObject() {
        XCTAssertEqual(string(AiPanelFormSubmission.bytes(orderedValues: [])), "{}\n")
    }

    func testSingleFieldRoundTrips() {
        XCTAssertEqual(
            string(AiPanelFormSubmission.bytes(orderedValues: [("name", "world")])),
            "{\"name\":\"world\"}\n"
        )
    }

    func testMultipleFieldsPreserveGivenOrder() {
        let bytes = AiPanelFormSubmission.bytes(orderedValues: [("b", "2"), ("a", "1")])
        XCTAssertEqual(string(bytes), "{\"b\":\"2\",\"a\":\"1\"}\n")
    }

    /// 決定性: 同じ順序の入力からは常に同じバイト列が得られる(内部でDictionaryの
    /// 不定なイテレーション順に依存していないことの確認)。
    func testSameOrderedInputIsDeterministicAcrossCalls() {
        let input: [(key: String, value: String)] = [("z", "1"), ("a", "2"), ("m", "3")]
        let first = AiPanelFormSubmission.bytes(orderedValues: input)
        let second = AiPanelFormSubmission.bytes(orderedValues: input)
        XCTAssertEqual(first, second)
    }

    func testEscapesQuoteBackslashAndNewline() {
        let bytes = AiPanelFormSubmission.bytes(orderedValues: [("k", "a\"b\\c\nd\re\tf")])
        XCTAssertEqual(string(bytes), "{\"k\":\"a\\\"b\\\\c\\nd\\re\\tf\"}\n")
    }

    func testEscapesOtherControlCharactersAsUnicodeEscape() {
        let bytes = AiPanelFormSubmission.bytes(orderedValues: [("k", "\u{0001}")])
        XCTAssertEqual(string(bytes), "{\"k\":\"\\u0001\"}\n")
    }

    func testNonAsciiValuesPassThroughAsUtf8() {
        let bytes = AiPanelFormSubmission.bytes(orderedValues: [("挨拶", "こんにちは🎉")])
        XCTAssertEqual(string(bytes), "{\"挨拶\":\"こんにちは🎉\"}\n")
    }
}
