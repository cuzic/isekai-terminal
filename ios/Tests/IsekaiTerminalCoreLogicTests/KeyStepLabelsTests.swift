import XCTest
@testable import IsekaiTerminalCoreLogic

/// Android版`KeyStepLabelsTest.kt`と同じ観点の検証。
final class KeyStepLabelsTests: XCTestCase {

    func testCtrlCharShowsCaretNotation() {
        XCTAssertEqual(KeyStep.ctrlChar("b").shortLabel, "^B")
        XCTAssertEqual(KeyStep.ctrlChar("B").shortLabel, "^B")
    }

    func testTextShowsLiteralText() {
        XCTAssertEqual(KeyStep.text("c").shortLabel, "c")
    }

    func testSpecialShowsFriendlyNameForKnownKeys() {
        XCTAssertEqual(KeyStep.special(.escape).shortLabel, "Esc")
        XCTAssertEqual(KeyStep.special(.arrowUp).shortLabel, "↑")
        XCTAssertEqual(KeyStep.special(.functionKey(5)).shortLabel, "F5")
    }

    func testPlaceholderRefShowsBraces() {
        XCTAssertEqual(KeyStep.placeholderRef("prefix").shortLabel, "{prefix}")
    }

    func testPreviewTextJoinsStepsWithSpacesTmuxNewWindow() {
        let steps: [KeyStep] = [.placeholderRef("prefix"), .text("c")]
        XCTAssertEqual(steps.previewText, "{prefix} c")
    }

    func testPreviewTextOfEmptyListIsEmptyString() {
        let steps: [KeyStep] = []
        XCTAssertEqual(steps.previewText, "")
    }

    func testSpecialKeyChoicesDoesNotIncludeDuplicateLabels() {
        let labels = SpecialKeyChoices.all.map(\.label)
        XCTAssertEqual(labels.count, Set(labels).count)
    }
}
