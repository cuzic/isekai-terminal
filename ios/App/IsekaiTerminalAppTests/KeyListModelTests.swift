import XCTest
@testable import IsekaiTerminalCore

/// Phase 1D: `KeyListModel`の検証。`CredentialVault`(Keychain)に触れるため、
/// `IsekaiTerminalCoreTests`(非ホスト)ではなくアプリホスト型の`IsekaiTerminalAppTests`に置く
/// (`CredentialVaultTests.swift`のコメント参照)。
@MainActor
final class KeyListModelTests: XCTestCase {
    private var tempDir: URL!
    private var db: ProfileDatabase!
    private var vault: CredentialVault!
    private var model: KeyListModel!

    override func setUpWithError() throws {
        tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        db = try ProfileDatabase.inMemory()
        vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.keylistmodel.\(UUID().uuidString)")
        model = KeyListModel(db: db, vault: vault)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: tempDir)
    }

    func testGenerateKeyStoresSecretAndMetadata() throws {
        model.generateKey(displayName: "my new key")

        XCTAssertNil(model.generateError)
        XCTAssertNotNil(model.generatedPublicKey)
        XCTAssertTrue(model.generatedPublicKey?.hasPrefix("ssh-ed25519 ") ?? false)
        XCTAssertEqual(model.keys.count, 1)
        XCTAssertEqual(model.keys.first?.displayName, "my new key")

        // 実際にVaultへ秘密材料が保存され、復号できることを確認する。
        let entry = try XCTUnwrap(model.keys.first)
        let metadata = CredentialVault.Metadata(keyId: entry.id, keyType: entry.keyType, publicKey: entry.publicKey)
        let secret = try vault.retrieve(metadata: metadata)
        let pemText = String(decoding: secret, as: UTF8.self)
        XCTAssertTrue(pemText.hasPrefix("-----BEGIN OPENSSH PRIVATE KEY-----"))
    }

    func testConfirmDeleteRemovesFromDbAndVault() throws {
        model.generateKey(displayName: "to delete")
        let entry = try XCTUnwrap(model.keys.first)

        model.requestDelete(entry)
        model.confirmDelete(entry)

        XCTAssertNil(model.pendingDelete)
        XCTAssertTrue(model.keys.isEmpty)
        XCTAssertTrue(try db.fetchAllKeyEntries().isEmpty)

        let metadata = CredentialVault.Metadata(keyId: entry.id, keyType: entry.keyType, publicKey: entry.publicKey)
        XCTAssertThrowsError(try vault.retrieve(metadata: metadata))
    }

    func testDismissGeneratedPublicKeyClearsState() throws {
        model.generateKey(displayName: "key")
        XCTAssertNotNil(model.generatedPublicKey)

        model.dismissGeneratedPublicKey()
        XCTAssertNil(model.generatedPublicKey)
    }
}
