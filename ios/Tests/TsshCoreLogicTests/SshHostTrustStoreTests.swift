import XCTest
@testable import TsshCoreLogic

/// Phase 1B: SSH/helper信頼ストアの検証。ファイルI/Oのみで完結し
/// entitlementを必要としないため、素のSwiftPMパッケージのテストバンドル
/// (TsshCoreTests)でそのまま検証できる(CredentialVaultとは異なり
/// TsshTerminalAppTestsへ置く必要はない)。
final class SshHostTrustStoreTests: XCTestCase {
    private var storeURL: URL!

    override func setUpWithError() throws {
        storeURL = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString)
            .appendingPathExtension("json")
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: storeURL)
    }

    func testUnknownHostOnFirstCheck() throws {
        let store = try SshHostTrustStore(storeURL: storeURL)
        let id = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "example.com", port: 22)

        XCTAssertEqual(store.verify(identifier: id, keyType: "ssh-ed25519", fingerprint: "AA:BB"), .unknownHost)
    }

    func testTrustedMatchAfterApproval() throws {
        let store = try SshHostTrustStore(storeURL: storeURL)
        let id = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "example.com", port: 22)

        try store.trust(identifier: id, keyType: "ssh-ed25519", fingerprint: "AA:BB")

        XCTAssertEqual(store.verify(identifier: id, keyType: "ssh-ed25519", fingerprint: "AA:BB"), .trustedMatch)
    }

    func testMismatchWhenFingerprintChanges() throws {
        let store = try SshHostTrustStore(storeURL: storeURL)
        let id = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "example.com", port: 22)
        try store.trust(identifier: id, keyType: "ssh-ed25519", fingerprint: "AA:BB")

        let result = store.verify(identifier: id, keyType: "ssh-ed25519", fingerprint: "CC:DD")

        XCTAssertEqual(result, .mismatch(previousFingerprint: "AA:BB", previousKeyType: "ssh-ed25519"))
    }

    func testMismatchDoesNotAutomaticallyOverwrite() throws {
        let store = try SshHostTrustStore(storeURL: storeURL)
        let id = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "example.com", port: 22)
        try store.trust(identifier: id, keyType: "ssh-ed25519", fingerprint: "AA:BB")

        _ = store.verify(identifier: id, keyType: "ssh-ed25519", fingerprint: "CC:DD")

        // verify()を呼んだだけでは上書きされない。明示的なtrust()呼び出しが必須。
        XCTAssertEqual(store.record(for: id)?.fingerprint, "AA:BB")
    }

    func testDifferentKindsWithSameHostPortAreIndependent() throws {
        let store = try SshHostTrustStore(storeURL: storeURL)
        let sshId = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "10.0.0.1", port: 8443)
        let helperId = SshHostTrustStore.makeIdentifier(kind: .isekaiHelperIdentity, host: "10.0.0.1", port: 8443)

        try store.trust(identifier: sshId, keyType: "ssh-ed25519", fingerprint: "AA:BB")

        XCTAssertEqual(store.verify(identifier: sshId, keyType: "ssh-ed25519", fingerprint: "AA:BB"), .trustedMatch)
        XCTAssertEqual(store.verify(identifier: helperId, keyType: "ssh-ed25519", fingerprint: "AA:BB"), .unknownHost)
    }

    func testHostnameCaseIsNormalized() {
        let lower = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "example.com", port: 22)
        let upper = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "EXAMPLE.COM", port: 22)

        XCTAssertEqual(lower, upper)
    }

    func testRevokeRemovesTrust() throws {
        let store = try SshHostTrustStore(storeURL: storeURL)
        let id = SshHostTrustStore.makeIdentifier(kind: .jumpHost, host: "bastion.example.com", port: 22)
        try store.trust(identifier: id, keyType: "ssh-rsa", fingerprint: "EE:FF")

        try store.revoke(identifier: id)

        XCTAssertEqual(store.verify(identifier: id, keyType: "ssh-rsa", fingerprint: "EE:FF"), .unknownHost)
    }

    func testPersistsAcrossInstances() throws {
        let first = try SshHostTrustStore(storeURL: storeURL)
        let id = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "persist.example.com", port: 22)
        try first.trust(identifier: id, keyType: "ssh-ed25519", fingerprint: "11:22")

        // 新しいインスタンス(=アプリ再起動を模擬)でも同じファイルから復元できる。
        let second = try SshHostTrustStore(storeURL: storeURL)
        XCTAssertEqual(second.verify(identifier: id, keyType: "ssh-ed25519", fingerprint: "11:22"), .trustedMatch)
    }
}
