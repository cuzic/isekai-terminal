import XCTest
@testable import IsekaiTerminalCoreLogic

/// iOS対応 Phase 0 の技術検証スパイク: Swift側からRustの `core_version()`
/// （`rust-core/src/lib.rs`）を呼び出すround-tripが成功することを確認する。
/// `PLAN.md` の「Phase Y」節 Phase 0-4 参照。
final class CoreVersionRoundTripTests: XCTestCase {
    func testCoreVersionMatchesCargoPackageVersion() {
        // rust-core/Cargo.toml の [package] version と一致する想定。
        XCTAssertEqual(coreVersion(), "0.1.0")
    }
}
