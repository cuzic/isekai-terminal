import XCTest
@testable import TsshCore

/// Phase 1D(#18b): `TerminalSessionController.onHostKey`のTOFU(Trust On First Use)
/// ロジックの検証。実際のSSH接続は行わず、ホスト鍵確認ロジックだけを直接呼び出す。
@MainActor
final class TerminalSessionControllerTests: XCTestCase {
    private func makeController(host: String = "127.0.0.1") throws -> (TerminalSessionController, SshHostTrustStore) {
        let db = try ProfileDatabase.inMemory()
        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        let vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.terminalsession.\(UUID().uuidString)")
        let trustStore = try SshHostTrustStore(storeURL: tempDir.appendingPathComponent("trust.json"))

        let profile = ConnectionProfile(displayName: "test", host: host, port: 22, username: "tester")
        let controller = TerminalSessionController(profile: profile, password: "pw", db: db, vault: vault, trustStore: trustStore)
        return (controller, trustStore)
    }

    func testFirstConnectionAutoTrustsAndAccepts() throws {
        let (controller, trustStore) = try makeController()

        XCTAssertTrue(controller.onHostKey(fingerprint: "SHA256:aaaa"))

        let identifier = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "127.0.0.1", port: 22)
        XCTAssertEqual(trustStore.record(for: identifier)?.fingerprint, "SHA256:aaaa")
    }

    func testSameFingerprintOnSecondConnectionIsAccepted() throws {
        let (controller, _) = try makeController()

        XCTAssertTrue(controller.onHostKey(fingerprint: "SHA256:aaaa"))
        XCTAssertTrue(controller.onHostKey(fingerprint: "SHA256:aaaa"))
    }

    func testChangedFingerprintIsRejectedAndSurfacesFailure() async throws {
        let (controller, _) = try makeController()

        XCTAssertTrue(controller.onHostKey(fingerprint: "SHA256:aaaa"))
        XCTAssertFalse(controller.onHostKey(fingerprint: "SHA256:bbbb"))

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.state != .connecting
        }
        guard case .failed = controller.uiState.state else {
            XCTFail("expected .failed state, got \(controller.uiState.state)")
            return
        }
    }

    // 実sshd接続+CredentialVault(Keychain)を伴うE2Eテストは、素のSwiftPM
    // テストバンドルではKeychainがerrSecMissingEntitlement(-34018)で失敗するため
    // (`CredentialVaultTests.swift`のコメント参照)、アプリホスト型の
    // `TsshTerminalAppTests`側に`TerminalSessionControllerE2ETests.swift`として
    // 置いている。

    // MARK: - Phase 1E-2: 踏み台(ProxyJump)の認証解決(実接続なし)

    func testConnectFailsWhenJumpKeyEntryNotFound() async throws {
        let db = try ProfileDatabase.inMemory()
        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        let vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.terminalsession.jump.\(UUID().uuidString)")
        let trustStore = try SshHostTrustStore(storeURL: tempDir.appendingPathComponent("trust.json"))

        let profile = ConnectionProfile(
            displayName: "via jump",
            host: "internal.example.com",
            port: 22,
            username: "user",
            jumpHost: "bastion.example.com",
            jumpPort: 22,
            jumpUsername: "jumpuser",
            jumpKeyEntryId: "does-not-exist"
        )
        let controller = TerminalSessionController(profile: profile, password: nil, db: db, vault: vault, trustStore: trustStore)

        controller.connect()

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.state != .connecting
        }
        guard case .failed(let message) = controller.uiState.state else {
            XCTFail("expected .failed state, got \(controller.uiState.state)")
            return
        }
        XCTAssertTrue(message.contains("踏み台"))
    }

    func testConnectFailsWhenMainKeyEntryNotFound() async throws {
        let db = try ProfileDatabase.inMemory()
        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        let vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.terminalsession.mainkey.\(UUID().uuidString)")
        let trustStore = try SshHostTrustStore(storeURL: tempDir.appendingPathComponent("trust.json"))

        let profile = ConnectionProfile(
            displayName: "test",
            host: "example.com",
            port: 22,
            username: "user",
            keyEntryId: "does-not-exist"
        )
        let controller = TerminalSessionController(profile: profile, password: nil, db: db, vault: vault, trustStore: trustStore)

        controller.connect()

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.state != .connecting
        }
        guard case .failed = controller.uiState.state else {
            XCTFail("expected .failed state, got \(controller.uiState.state)")
            return
        }
    }

    // MARK: - Phase 1E-4: SSH agent署名要求の確認フロー

    func testAgentSignRequestApprovedReturnsTrue() async throws {
        let (controller, _) = try makeController()

        let resultTask = Task.detached {
            controller.onAgentSignRequest(keyFingerprint: "SHA256:cccc")
        }

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.pendingAgentSignRequest != nil
        }
        XCTAssertEqual(controller.uiState.pendingAgentSignRequest?.fingerprint, "SHA256:cccc")
        controller.respondToAgentSignRequest(approved: true)

        let result = await resultTask.value
        XCTAssertTrue(result)
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.pendingAgentSignRequest == nil
        }
    }

    func testAgentSignRequestDeniedReturnsFalse() async throws {
        let (controller, _) = try makeController()

        let resultTask = Task.detached {
            controller.onAgentSignRequest(keyFingerprint: "SHA256:dddd")
        }

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.pendingAgentSignRequest != nil
        }
        controller.respondToAgentSignRequest(approved: false)

        let result = await resultTask.value
        XCTAssertFalse(result)
    }
}
