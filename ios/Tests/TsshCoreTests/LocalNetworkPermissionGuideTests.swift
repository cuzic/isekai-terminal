import XCTest
@testable import TsshCore

final class LocalNetworkPermissionGuideTests: XCTestCase {
    func testAppSettingsURLIsValid() {
        XCTAssertNotNil(LocalNetworkPermissionGuide.appSettingsURL)
    }
}
