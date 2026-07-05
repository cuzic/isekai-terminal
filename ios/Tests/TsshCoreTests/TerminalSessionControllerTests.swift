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
}
