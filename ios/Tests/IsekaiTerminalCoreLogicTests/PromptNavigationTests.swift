import XCTest
@testable import IsekaiTerminalCoreLogic

/// Y-P1(#5): `PromptNavigation`の検証(`docs/adr/0001-ios-parity-implementation.md` §3.1)。
final class PromptNavigationTests: XCTestCase {
    func testScrollTargetForNilReturnsNil() {
        XCTAssertNil(PromptNavigation.scrollTarget(for: nil))
    }

    func testScrollTargetForLiveTargetResetsToLiveDisplay() {
        let target = PromptJumpTarget(scrollOffset: 42, isLive: true)
        XCTAssertEqual(
            PromptNavigation.scrollTarget(for: target),
            PromptNavigation.ScrollTarget(scrollOffset: 0, showingScrollback: false)
        )
    }

    func testScrollTargetForScrollbackTargetUsesOffsetAndShowsScrollback() {
        let target = PromptJumpTarget(scrollOffset: 17, isLive: false)
        XCTAssertEqual(
            PromptNavigation.scrollTarget(for: target),
            PromptNavigation.ScrollTarget(scrollOffset: 17, showingScrollback: true)
        )
    }

    func testNotFoundMessageIsNilWhenTargetPresent() {
        XCTAssertNil(PromptNavigation.notFoundMessage(for: PromptJumpTarget(scrollOffset: 0, isLive: true)))
    }

    func testNotFoundMessageIsNonEmptyWhenTargetNil() {
        XCTAssertEqual(PromptNavigation.notFoundMessage(for: nil), "前後にジャンプ可能なプロンプトが見つかりませんでした")
    }
}
