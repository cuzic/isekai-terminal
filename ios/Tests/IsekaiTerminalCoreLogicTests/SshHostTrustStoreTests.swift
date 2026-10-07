import XCTest
@testable import IsekaiTerminalCoreLogic

/// Phase 1B: SSH/helper信頼ストアの検証。ファイルI/Oのみで完結し
/// entitlementを必要としないため、素のSwiftPMパッケージのテストバンドル
/// (IsekaiTerminalCoreTests)でそのまま検証できる(CredentialVaultとは異なり
/// IsekaiTerminalAppTestsへ置く必要はない)。
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

    // MARK: - 2026-09-29レビュー IOS-I2/I3

    /// 同じidentifierが重複したファイルでも初期化がtrapせず、後勝ちで読める
    /// (以前は`Dictionary(uniqueKeysWithValues:)`が実行時trapし、起動のたびにクラッシュした)。
    func testDuplicateIdentifiersInFileLoadLastWins() throws {
        let json = """
        [
          {"identifier": "sshHost|dup.example.com:22", "keyType": "ssh", "fingerprint": "SHA256:old", "firstTrustedAt": 0},
          {"identifier": "sshHost|dup.example.com:22", "keyType": "ssh", "fingerprint": "SHA256:new", "firstTrustedAt": 1}
        ]
        """
        try Data(json.utf8).write(to: storeURL)

        let store = try SshHostTrustStore(storeURL: storeURL)

        XCTAssertEqual(store.allRecords.count, 1)
        XCTAssertEqual(
            store.verify(identifier: "sshHost|dup.example.com:22", keyType: "ssh", fingerprint: "SHA256:new"),
            .trustedMatch
        )
    }

    /// 壊れたJSONでも`openRecoveringCorruption`は空のストアで開き、壊れたファイルを退避する。
    func testOpenRecoveringCorruptionQuarantinesBrokenFileAndStartsEmpty() throws {
        try Data("{not json".utf8).write(to: storeURL)
        XCTAssertThrowsError(try SshHostTrustStore(storeURL: storeURL))

        let (store, recovered) = SshHostTrustStore.openRecoveringCorruption(storeURL: storeURL)

        let quarantined = try XCTUnwrap(try XCTUnwrap(recovered).quarantinedURL)
        defer { try? FileManager.default.removeItem(at: quarantined) }
        XCTAssertTrue(FileManager.default.fileExists(atPath: quarantined.path))
        XCTAssertEqual(try Data(contentsOf: quarantined), Data("{not json".utf8))
        XCTAssertFalse(FileManager.default.fileExists(atPath: storeURL.path))
        XCTAssertTrue(store.allRecords.isEmpty)

        // 開き直したストアはそのまま使え、同じパスへ正常に永続化できる。
        let id = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "after.example.com", port: 22)
        try store.trust(identifier: id, keyType: "ssh", fingerprint: "SHA256:aaaa")
        XCTAssertEqual(
            try SshHostTrustStore(storeURL: storeURL).verify(identifier: id, keyType: "ssh", fingerprint: "SHA256:aaaa"),
            .trustedMatch
        )
    }

    func testOpenRecoveringCorruptionReportsNothingForHealthyOrMissingFile() throws {
        let (_, recoveredMissing) = SshHostTrustStore.openRecoveringCorruption(storeURL: storeURL)
        XCTAssertNil(recoveredMissing)
    }

    /// 永続化に失敗した`trust`はメモリ上の状態も変えない(ファイルとの食い違いを作らない)。
    func testTrustDoesNotMutateMemoryWhenSaveFails() throws {
        let unwritable = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
            .appendingPathComponent("missing-dir", isDirectory: true)
            .appendingPathComponent("trust.json")
        let store = try SshHostTrustStore(storeURL: unwritable)
        let id = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "nosave.example.com", port: 22)

        XCTAssertThrowsError(try store.trust(identifier: id, keyType: "ssh", fingerprint: "SHA256:aaaa"))

        XCTAssertEqual(store.verify(identifier: id, keyType: "ssh", fingerprint: "SHA256:aaaa"), .unknownHost)
        XCTAssertNil(store.record(for: id))
    }

    /// 複数スレッド(Rustのコールバックスレッド群+main相当)からの同時verify/trustで
    /// クラッシュせず、全ての書き込みが反映される。
    func testConcurrentVerifyAndTrustAreSafe() throws {
        let store = try SshHostTrustStore(storeURL: storeURL)
        let iterations = 200

        DispatchQueue.concurrentPerform(iterations: iterations) { i in
            let id = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "host\(i).example.com", port: 22)
            _ = store.verify(identifier: id, keyType: "ssh", fingerprint: "SHA256:\(i)")
            try? store.trust(identifier: id, keyType: "ssh", fingerprint: "SHA256:\(i)")
            _ = store.allRecords
        }

        XCTAssertEqual(store.allRecords.count, iterations)
        let reloaded = try SshHostTrustStore(storeURL: storeURL)
        XCTAssertEqual(reloaded.allRecords.count, iterations)
    }
}
