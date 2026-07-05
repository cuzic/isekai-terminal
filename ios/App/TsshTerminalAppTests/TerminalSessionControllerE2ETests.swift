import XCTest
@testable import TsshCore

/// Phase 1D(#18b): `TerminalSessionController`が実sshd(CI fixture)へ実際に接続し、
/// `onConnected`/`onScreenUpdate`が`TerminalUIState`まで届くことを検証する
/// (`SshVerticalSliceTests`/`KeyManagerTests`と同じ「実際のプロトコル互換性を
/// 検証する」方針)。鍵認証プロファイル(`CredentialVault`経由の秘密鍵解決)の
/// 経路を通るため、Keychainに触れる。素の`TsshCoreTests`ではKeychainが
/// `errSecMissingEntitlement`(-34018)で失敗するため、アプリホスト型の
/// このターゲットに置く(`CredentialVaultTests.swift`と同じ理由)。
@MainActor
final class TerminalSessionControllerE2ETests: XCTestCase {
    func testConnectReceivesRealScreenUpdateFromFixture() async throws {
        guard let fixture = try? E2EFixtureConfig.load() else {
            throw XCTSkip("SSH fixture not available at \(E2EFixtureConfig.defaultPath); run start-sshd-fixture.sh first")
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

        try await waitUntilE2ECondition(timeout: 10) {
            await controller.uiState.state == .connected
        }

        controller.send(Data("echo hello\n".utf8))

        try await waitUntilE2ECondition(timeout: 10) {
            await controller.uiState.latestScreenUpdate != nil
        }

        controller.disconnect()

        try await waitUntilE2ECondition(timeout: 10) {
            if case .disconnected = await controller.uiState.state { return true }
            return false
        }
    }
}

/// `ios/Tests/TsshCoreTests/SshFixtureConfig.swift`と同内容だが、別テストターゲット
/// (別モジュール)のため共有できず複製している。
private struct E2EFixtureConfig: Decodable {
    static let defaultPath = "/tmp/ios-fixture/fixture.json"

    let host: String
    let port: Int
    let user: String
    let privateKeyPath: String

    enum CodingKeys: String, CodingKey {
        case host, port, user
        case privateKeyPath = "private_key_path"
    }

    static func load() throws -> E2EFixtureConfig {
        let path = ProcessInfo.processInfo.environment["IOS_SSH_FIXTURE_JSON"] ?? defaultPath
        let data = try Data(contentsOf: URL(fileURLWithPath: path))
        return try JSONDecoder().decode(E2EFixtureConfig.self, from: data)
    }
}

private struct E2ETimeoutError: Error {}

private func waitUntilE2ECondition(timeout: TimeInterval, condition: () async -> Bool) async throws {
    let deadline = Date().addingTimeInterval(timeout)
    while Date() < deadline {
        if await condition() { return }
        try await Task.sleep(nanoseconds: 50_000_000)
    }
    throw E2ETimeoutError()
}
