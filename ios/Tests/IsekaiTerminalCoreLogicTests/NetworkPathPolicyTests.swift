import XCTest
@testable import IsekaiTerminalCoreLogic

/// Phase 1B: NetworkPathPolicy/NetworkPathObserverの検証。実際のNWPathMonitorの
/// ネットワーク切替そのものはCI/シミュレータでは再現できないため、判断ロジック
/// (「いつRustへ通知するか」)だけを単体で検証する。
final class NetworkPathPolicyTests: XCTestCase {
    func testHealthyConnectionAlwaysDebounces() {
        let policy = NetworkPathPolicy(defaultDebounceInterval: 0.3)

        XCTAssertEqual(policy.decide(isSatisfied: true, health: .healthy), .notifyAfterDebounce(interval: 0.3))
        XCTAssertEqual(policy.decide(isSatisfied: false, health: .healthy), .notifyAfterDebounce(interval: 0.3))
    }

    func testDegradedConnectionNotifiesImmediatelyWhenSatisfied() {
        let policy = NetworkPathPolicy()

        XCTAssertEqual(policy.decide(isSatisfied: true, health: .degradedOrReconnecting), .notifyImmediately)
    }

    func testDegradedConnectionIgnoresUnsatisfiedUpdates() {
        let policy = NetworkPathPolicy()

        XCTAssertEqual(policy.decide(isSatisfied: false, health: .degradedOrReconnecting), .ignore)
    }
}

final class NetworkPathObserverTests: XCTestCase {
    func testImmediateNotificationFiresRightAway() {
        var notifications: [(UInt64, Bool)] = []
        let observer = NetworkPathObserver { epoch, isSatisfied in
            notifications.append((epoch, isSatisfied))
        }

        let decision = observer.handlePathUpdate(isSatisfied: true, health: .degradedOrReconnecting)

        XCTAssertEqual(decision, .notifyImmediately)
        XCTAssertEqual(notifications.count, 1)
        XCTAssertEqual(notifications.first?.0, 1)
        XCTAssertEqual(notifications.first?.1, true)
    }

    func testEpochIncrementsOnEveryUpdate() {
        let observer = NetworkPathObserver { _, _ in }

        XCTAssertEqual(observer.epoch, 0)
        _ = observer.handlePathUpdate(isSatisfied: true, health: .degradedOrReconnecting)
        XCTAssertEqual(observer.epoch, 1)
        _ = observer.handlePathUpdate(isSatisfied: true, health: .degradedOrReconnecting)
        XCTAssertEqual(observer.epoch, 2)
    }

    func testDebouncedNotificationFiresAfterDelay() async throws {
        var notifications: [(UInt64, Bool)] = []
        let observer = NetworkPathObserver(policy: NetworkPathPolicy(defaultDebounceInterval: 0.05)) { epoch, isSatisfied in
            notifications.append((epoch, isSatisfied))
        }

        _ = observer.handlePathUpdate(isSatisfied: true, health: .healthy)
        XCTAssertTrue(notifications.isEmpty, "debounce前は通知されないはず")

        try await Task.sleep(nanoseconds: 200_000_000) // 200ms > 50msのdebounce
        XCTAssertEqual(notifications.count, 1)
    }

    func testNewerUpdateCancelsPendingDebouncedNotification() async throws {
        var notifications: [(UInt64, Bool)] = []
        let observer = NetworkPathObserver(policy: NetworkPathPolicy(defaultDebounceInterval: 0.05)) { epoch, isSatisfied in
            notifications.append((epoch, isSatisfied))
        }

        _ = observer.handlePathUpdate(isSatisfied: true, health: .healthy) // epoch 1、debounce待ちになる
        _ = observer.handlePathUpdate(isSatisfied: false, health: .healthy) // epoch 2、1をキャンセルするはず

        try await Task.sleep(nanoseconds: 200_000_000)

        // epoch 1の古い通知は発火せず、epoch 2の通知だけが1件発火する。
        XCTAssertEqual(notifications.count, 1)
        XCTAssertEqual(notifications.first?.0, 2)
        XCTAssertEqual(notifications.first?.1, false)
    }
}
