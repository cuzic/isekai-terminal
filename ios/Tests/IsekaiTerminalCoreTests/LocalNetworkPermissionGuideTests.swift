import XCTest
@testable import IsekaiTerminalCore

final class LocalNetworkPermissionGuideTests: XCTestCase {
    func testAppSettingsURLIsValid() {
        XCTAssertNotNil(LocalNetworkPermissionGuide.appSettingsURL)
    }
}
