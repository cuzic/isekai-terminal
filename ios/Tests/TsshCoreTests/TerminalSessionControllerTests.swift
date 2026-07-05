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

    /// 実sshd(CI fixture)へ実際に接続し、`onConnected`/`onScreenUpdate`が
    /// `TerminalUIState`まで届くことを検証する(SshVerticalSliceTests/
    /// KeyManagerTestsと同じ「実際のプロトコル互換性を検証する」方針)。
    /// 鍵認証プロファイル(CredentialVault経由の秘密鍵解決)の経路もここで検証する。
    func testConnectReceivesRealScreenUpdateFromFixture() async throws {
        guard let fixture = try? SshFixtureConfig.load() else {
            throw XCTSkip("SSH fixture not available at \(SshFixtureConfig.defaultPath); run start-sshd-fixture.sh first")
        }

        let db = try ProfileDatabase.inMemory()
        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        let vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.terminalsession.e2e.\(UUID().uuidString)")
        let trustStore = try SshHostTrustStore(storeURL: tempDir.appendingPathComponent("trust.json"))

        // fixtureの秘密鍵をCredentialVaultへ保存し、KeyEntryとしてDBに登録する
        // (実アプリのフロー: プロファイルはkeyEntryIdだけを持ち、TerminalSessionController
        // がそれをCredentialVaultで解決する)。
        let privateKeyPem = try Data(contentsOf: URL(fileURLWithPath: fixture.privateKeyPath))
        let keyId = "fixture-key"
        let metadata = CredentialVault.Metadata(keyId: keyId, keyType: "ed25519", publicKey: "fixture")
        try vault.store(secret: privateKeyPem, metadata: metadata)
        try db.insert(keyEntry: KeyEntry(id: keyId, displayName: "fixture", keyType: "ed25519", publicKey: "fixture"))

        var profile = ConnectionProfile(displayName: "fixture", host: fixture.host, port: fixture.port, username: fixture.user, keyEntryId: keyId)
        try db.insert(profile: &profile)

        let controller = TerminalSessionController(profile: profile, password: nil, db: db, vault: vault, trustStore: trustStore)
        controller.connect()

        try await waitUntilFixtureCondition(timeout: 10) {
            await controller.uiState.state == .connected
        }

        controller.send(Data("echo hello\n".utf8))

        try await waitUntilFixtureCondition(timeout: 10) {
            await controller.uiState.latestScreenUpdate != nil
        }

        controller.disconnect()

        try await waitUntilFixtureCondition(timeout: 10) {
            if case .disconnected = await controller.uiState.state { return true }
            return false
        }
    }
}
