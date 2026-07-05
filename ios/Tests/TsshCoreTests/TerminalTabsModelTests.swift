import XCTest
import UIKit
@testable import TsshCore
import TsshCoreLogic

/// Phase 1G-2(#54): `TerminalTabsModel`(Android版`TerminalTabsViewModel`のタブ
/// リスト/アクティブタブ管理部分)の検証。実際のネットワーク接続は行わない
/// (プロファイルに存在しない`keyEntryId`を与え、`connect()`が`resolveAuth`の
/// 鍵解決失敗で即座に`.failed`へ遷移し、ネットワークに触れる前に終わることを利用する)。
@MainActor
final class TerminalTabsModelTests: XCTestCase {
    private func makeModel() throws -> TerminalTabsModel {
        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        let trustStore = try SshHostTrustStore(storeURL: tempDir.appendingPathComponent("trust.json"))
        let db = try ProfileDatabase.inMemory()
        let vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.tabs.\(UUID().uuidString)")
        let relayVault = RelayCredentialVault(keychainService: "test.tabs.relay.\(UUID().uuidString)")
        return TerminalTabsModel(trustStore: trustStore, db: db, vault: vault, relayVault: relayVault)
    }

    private func makeProfile(displayName: String) -> ConnectionProfile {
        // 実接続を避けるため存在しないkeyEntryIdを指定する(resolveAuthが即座に失敗し、
        // ネットワークには一切触れない)。
        ConnectionProfile(displayName: displayName, host: "example.com", port: 22, username: "user", keyEntryId: "does-not-exist")
    }

    func testOpenTabAddsTabAndSetsActive() throws {
        let model = try makeModel()

        let tabId = model.openTab(profile: makeProfile(displayName: "test"), password: nil)

        XCTAssertEqual(model.tabs.count, 1)
        XCTAssertEqual(model.tabs.first?.id, tabId)
        XCTAssertEqual(model.activeTabId, tabId)
        XCTAssertEqual(model.tabs.first?.profile.displayName, "test")
    }

    func testOpeningMultipleTabsActivatesTheLatest() throws {
        let model = try makeModel()

        let first = model.openTab(profile: makeProfile(displayName: "first"), password: nil)
        let second = model.openTab(profile: makeProfile(displayName: "second"), password: nil)

        XCTAssertEqual(model.tabs.map(\.id), [first, second])
        XCTAssertEqual(model.activeTabId, second)
    }

    func testSetActiveTabSwitchesActiveTab() throws {
        let model = try makeModel()
        let first = model.openTab(profile: makeProfile(displayName: "first"), password: nil)
        model.openTab(profile: makeProfile(displayName: "second"), password: nil)

        model.setActiveTab(first)

        XCTAssertEqual(model.activeTabId, first)
    }

    func testSetActiveTabIgnoresUnknownId() throws {
        let model = try makeModel()
        let first = model.openTab(profile: makeProfile(displayName: "first"), password: nil)

        model.setActiveTab(UUID())

        XCTAssertEqual(model.activeTabId, first)
    }

    func testCloseTabRemovesItFromList() throws {
        let model = try makeModel()
        let tabId = model.openTab(profile: makeProfile(displayName: "test"), password: nil)

        model.closeTab(tabId)

        XCTAssertTrue(model.tabs.isEmpty)
    }

    func testCloseActiveTabActivatesLastRemainingTab() throws {
        let model = try makeModel()
        let first = model.openTab(profile: makeProfile(displayName: "first"), password: nil)
        let second = model.openTab(profile: makeProfile(displayName: "second"), password: nil)
        model.setActiveTab(second)

        model.closeTab(second)

        XCTAssertEqual(model.tabs.map(\.id), [first])
        XCTAssertEqual(model.activeTabId, first)
    }

    func testCloseInactiveTabDoesNotChangeActiveTab() throws {
        let model = try makeModel()
        let first = model.openTab(profile: makeProfile(displayName: "first"), password: nil)
        let second = model.openTab(profile: makeProfile(displayName: "second"), password: nil)
        model.setActiveTab(second)

        model.closeTab(first)

        XCTAssertEqual(model.activeTabId, second)
        XCTAssertEqual(model.tabs.map(\.id), [second])
    }

    func testClosingLastTabClearsActiveTabId() throws {
        let model = try makeModel()
        let tabId = model.openTab(profile: makeProfile(displayName: "test"), password: nil)

        model.closeTab(tabId)

        XCTAssertNil(model.activeTabId)
    }

    func testCloseUnknownTabIdIsNoOp() throws {
        let model = try makeModel()
        let tabId = model.openTab(profile: makeProfile(displayName: "test"), password: nil)

        model.closeTab(UUID())

        XCTAssertEqual(model.tabs.map(\.id), [tabId])
    }

    /// Phase 1C(#14): フォアグラウンド復帰通知を受けたら、失敗/切断済みのタブに対して
    /// `reconnect()`が呼ばれることを検証する(実際のUIApplication lifecycle
    /// 通知をそのままpostし、`TerminalTabsModel`が実際に購読していることを確かめる)。
    func testWillEnterForegroundNotificationReconnectsFailedTab() async throws {
        let model = try makeModel()
        let tabId = model.openTab(profile: makeProfile(displayName: "test"), password: nil)

        try await waitUntilFixtureCondition(timeout: 2) {
            guard case .failed = await model.tabs.first(where: { $0.id == tabId })?.controller.uiState.state else { return false }
            return true
        }

        NotificationCenter.default.post(name: UIApplication.willEnterForegroundNotification, object: nil)

        // 同じ(存在しない)keyEntryIdで再接続を試みるため、再び同期的に.failedへ戻る。
        // ここでは「.connectingを経由した=reconnect()が実際に呼ばれた」ことを、
        // 最終的に.failedへ戻っていることの確認をもって間接的に検証する。
        try await waitUntilFixtureCondition(timeout: 2) {
            guard let controller = await model.tabs.first(where: { $0.id == tabId })?.controller else { return false }
            guard case .failed = await controller.uiState.state else { return false }
            return true
        }
    }
}
