import XCTest
@testable import TsshCore
import TsshCoreLogic

/// Phase 1G-1(#53): GRDBの`Snippet`レコード型に依存する`SnippetCommands.toBytes(snippet:)`
/// オーバーロードの検証。`command`/`appendNewline`の変換ロジック本体の検証は
/// `Tests/TsshCoreLogicTests/SnippetCommandsTests.swift`(Linuxでも実行可能)側にある。
final class SnippetCommandsTests: XCTestCase {
    func testSnippetOverloadUsesSnippetFields() {
        let snippet = Snippet(label: "test", command: "echo hi", appendNewline: false)
        let bytes = SnippetCommands.toBytes(snippet: snippet)
        XCTAssertEqual(String(data: bytes, encoding: .utf8), "echo hi")
    }
}
