import XCTest
@testable import IsekaiTerminalCoreLogic

/// Y-P1(#8): `SnippetTemplates.all`の妥当性検証(`ADR_IOS_PARITY_IMPLEMENTATION.md` §3.3)。
final class SnippetTemplatesTests: XCTestCase {
    func testAllTemplatesAreNonEmpty() {
        XCTAssertFalse(SnippetTemplates.all.isEmpty)
    }

    func testNoDuplicateLabels() {
        let labels = SnippetTemplates.all.map(\.label)
        XCTAssertEqual(labels.count, Set(labels).count, "duplicate template labels: \(labels)")
    }

    func testNoEmptyLabelsOrCommands() {
        for template in SnippetTemplates.all {
            XCTAssertFalse(template.label.trimmingCharacters(in: .whitespaces).isEmpty, "empty label")
            XCTAssertFalse(template.command.trimmingCharacters(in: .whitespaces).isEmpty, "empty command for '\(template.label)'")
        }
    }
}
