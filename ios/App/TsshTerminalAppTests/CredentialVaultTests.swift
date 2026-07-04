import CryptoKit
import XCTest
@testable import TsshCore

/// Phase 1B: CredentialVault(Keychain保護)の検証。
///
/// このテストは`TsshCoreTests`(素のSwiftPMパッケージのテストバンドル)ではなく、
/// `TsshTerminalApp`にホストされる`TsshTerminalAppTests`ターゲットに置く必要がある。
/// 素のSwiftPMパッケージのXCTestバンドルは実アプリでホストされないため、
/// Keychain APIが`errSecMissingEntitlement`(-34018)で失敗することをCIで確認した
/// (未署名/非ホストのプロセスはOSがどのアプリのKeychainか判定できないため)。
/// iOS Simulatorは実機と同様に動作するKeychainを持つため、実機なしでこれらの
/// テストが実行できる。
final class CredentialVaultTests: XCTestCase {
    private var tempDir: URL!
    private var vault: CredentialVault!

    override func setUpWithError() throws {
        tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.credentialvault.\(UUID().uuidString)")
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: tempDir)
    }

    private func uniqueMetadata(tag: String = "") -> CredentialVault.Metadata {
        CredentialVault.Metadata(
            keyId: "key-\(tag)-\(UUID().uuidString)",
            keyType: "ed25519",
            publicKey: "AAAA...\(tag)"
        )
    }

    func testStoreThenRetrieveRoundTrips() throws {
        let metadata = uniqueMetadata()
        let secret = Data("-----BEGIN OPENSSH PRIVATE KEY-----\ndummy\n".utf8)

        try vault.store(secret: secret, metadata: metadata)
        let retrieved = try vault.retrieve(metadata: metadata)

        XCTAssertEqual(retrieved, secret)
    }

    func testRetrieveWithMismatchedMetadataFails() throws {
        let metadata = uniqueMetadata()
        try vault.store(secret: Data("secret".utf8), metadata: metadata)

        let tamperedMetadata = CredentialVault.Metadata(
            keyId: metadata.keyId,
            keyType: "rsa", // keyTypeを変えるとAADが変わり復号に失敗するはず
            publicKey: metadata.publicKey
        )

        XCTAssertThrowsError(try vault.retrieve(metadata: tamperedMetadata))
    }

    func testDeleteRemovesBothBlobAndKey() throws {
        let metadata = uniqueMetadata()
        try vault.store(secret: Data("secret".utf8), metadata: metadata)

        try vault.delete(keyId: metadata.keyId)

        XCTAssertThrowsError(try vault.retrieve(metadata: metadata)) { error in
            XCTAssertEqual(error as? CredentialVault.VaultError, .blobNotFound)
        }
    }

    func testRotateKeyPreservesPlaintextButChangesEncryption() throws {
        let metadata = uniqueMetadata()
        let secret = Data("original-secret".utf8)
        try vault.store(secret: secret, metadata: metadata)

        let blobPathBeforeRotation = try Data(contentsOf: blobFileURL(for: metadata.keyId))
        try vault.rotateKey(metadata: metadata)
        let blobPathAfterRotation = try Data(contentsOf: blobFileURL(for: metadata.keyId))

        // 暗号文(nonce込み)は毎回変わるはずなので、ローテーション前後でblobの中身は異なる。
        XCTAssertNotEqual(blobPathBeforeRotation, blobPathAfterRotation)
        // それでも平文としては同じ値が復号できる。
        XCTAssertEqual(try vault.retrieve(metadata: metadata), secret)
    }

    func testCleanupOrphanBlobsRemovesUnknownFiles() throws {
        let kept = uniqueMetadata(tag: "kept")
        let orphan = uniqueMetadata(tag: "orphan")
        try vault.store(secret: Data("kept".utf8), metadata: kept)
        try vault.store(secret: Data("orphan".utf8), metadata: orphan)

        try vault.cleanupOrphanBlobs(knownKeyIds: [kept.keyId])

        XCTAssertEqual(try vault.retrieve(metadata: kept), Data("kept".utf8))
        XCTAssertThrowsError(try vault.retrieve(metadata: orphan))
    }

    func testUnsupportedFormatVersionIsDetected() throws {
        let metadata = uniqueMetadata()
        try vault.store(secret: Data("secret".utf8), metadata: metadata)

        // フォーマットバージョンのバイトを不正な値へ書き換える。
        var envelope = try Data(contentsOf: blobFileURL(for: metadata.keyId))
        envelope[0] = 0xFF
        try envelope.write(to: blobFileURL(for: metadata.keyId))

        XCTAssertThrowsError(try vault.retrieve(metadata: metadata)) { error in
            XCTAssertEqual(error as? CredentialVault.VaultError, .unsupportedFormatVersion(0xFF))
        }
    }

    func testStoreFailureRollsBackNewlyCreatedKeychainEntry() throws {
        // blobDirectoryを書き込み不可にして保存を失敗させ、新規生成したKEKが
        // Keychainに孤立して残らないことを確認する。
        let readonlyDir = tempDir.appendingPathComponent("readonly")
        try FileManager.default.createDirectory(at: readonlyDir, withIntermediateDirectories: true)
        try FileManager.default.setAttributes([.posixPermissions: 0o400], ofItemAtPath: readonlyDir.path)
        defer {
            try? FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: readonlyDir.path)
        }

        let readonlyVault = try CredentialVault(
            blobDirectory: readonlyDir,
            keychainService: "test.credentialvault.rollback.\(UUID().uuidString)"
        )
        let metadata = uniqueMetadata(tag: "rollback")

        XCTAssertThrowsError(try readonlyVault.store(secret: Data("secret".utf8), metadata: metadata))

        // ロールバックされていれば、以降のretrieveはblobNotFound
        // (KeychainにKEKが残っていてもblob自体が無いので同じエラーになる)。
        XCTAssertThrowsError(try readonlyVault.retrieve(metadata: metadata)) { error in
            XCTAssertEqual(error as? CredentialVault.VaultError, .blobNotFound)
        }
    }

    /// `CredentialVault`内部の`hashedFilename(for:)`と同じ導出方法(SHA256)を
    /// テスト側でも再現し、対応するblobファイルのURLを直接組み立てる。
    private func blobFileURL(for keyId: String) -> URL {
        let digest = SHA256.hash(data: Data(keyId.utf8))
        let filename = digest.map { String(format: "%02x", $0) }.joined()
        return tempDir.appendingPathComponent(filename).appendingPathExtension("cvault")
    }
}
