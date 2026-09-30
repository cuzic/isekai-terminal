import XCTest
@testable import IsekaiTerminalCore

/// 2026-09-29レビューIOS-L3: `RemoteClipboardBridge`は`UIPasteboard`へのアクセスを
/// main threadへ寄せる。Rustのコールバックスレッド相当(main以外)から呼んでも、
/// 本体はmainで実行され、値が呼び出し元へ返ることを検証する(実際のUIPasteboardには触れない)。
final class RemoteClipboardBridgeTests: XCTestCase {
    func testReadOnMainFromBackgroundThreadRunsBodyOnMainAndReturnsValue() async {
        let result: (value: String?, ranOnMain: Bool) = await withCheckedContinuation { continuation in
            DispatchQueue.global().async {
                var ranOnMain = false
                let value = RemoteClipboardBridge.readOnMain(timeout: .seconds(5)) { () -> String? in
                    ranOnMain = Thread.isMainThread
                    return "from-main"
                }
                continuation.resume(returning: (value, ranOnMain))
            }
        }

        XCTAssertEqual(result.value, "from-main")
        XCTAssertTrue(result.ranOnMain)
    }

    func testReadOnMainOnMainThreadRunsInline() {
        XCTAssertTrue(Thread.isMainThread)
        XCTAssertEqual(RemoteClipboardBridge.readOnMain(timeout: .seconds(1)) { 42 }, 42)
    }

    func testPerformOnMainFromBackgroundThreadRunsOnMain() {
        let done = expectation(description: "performed on main")
        DispatchQueue.global().async {
            RemoteClipboardBridge.performOnMain {
                XCTAssertTrue(Thread.isMainThread)
                done.fulfill()
            }
        }
        wait(for: [done], timeout: 5)
    }
}
