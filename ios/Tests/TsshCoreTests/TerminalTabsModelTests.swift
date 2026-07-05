import XCTest
@testable import TsshCore

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
}
