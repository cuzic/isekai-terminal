import XCTest
@testable import TsshCore

/// Phase 1D: `KeyImportModel`の検証。`CredentialVault`(Keychain)に触れるため、
/// `TsshCoreTests`(非ホスト)ではなくアプリホスト型の`TsshTerminalAppTests`に置く
/// (`CredentialVaultTests.swift`のコメント参照)。
@MainActor
final class KeyImportModelTests: XCTestCase {
    private var tempDir: URL!
    private var db: ProfileDatabase!
    private var vault: CredentialVault!
    private var model: KeyImportModel!

    override func setUpWithError() throws {
        tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        db = try ProfileDatabase.inMemory()
        vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.keyimportmodel.\(UUID().uuidString)")
        model = KeyImportModel(db: db, vault: vault)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: tempDir)
    }

    func testImportFailsWithEmptyLabel() {
        let pem = Data("-----BEGIN OPENSSH PRIVATE KEY-----\ndummy\n-----END OPENSSH PRIVATE KEY-----\n".utf8)
        XCTAssertFalse(model.importKey(pemBytes: pem, displayName: ""))
        XCTAssertNotNil(model.errorMessage)
    }

    func testImportFailsWithEmptyPem() {
        XCTAssertFalse(model.importKey(pemBytes: Data(), displayName: "my key"))
        XCTAssertNotNil(model.errorMessage)
    }

    func testImportSucceedsAndStoresSecret() throws {
        let pem = Data("-----BEGIN OPENSSH PRIVATE KEY-----\ndummy\n-----END OPENSSH PRIVATE KEY-----\n".utf8)

        XCTAssertTrue(model.importKey(pemBytes: pem, displayName: "imported key"))
        XCTAssertNil(model.errorMessage)

        let entries = try db.fetchAllKeyEntries()
        XCTAssertEqual(entries.count, 1)
        XCTAssertEqual(entries.first?.displayName, "imported key")
        XCTAssertEqual(entries.first?.keyType, "imported")

        let entry = try XCTUnwrap(entries.first)
        let metadata = CredentialVault.Metadata(keyId: entry.id, keyType: entry.keyType, publicKey: entry.publicKey)
        let secret = try vault.retrieve(metadata: metadata)
        XCTAssertEqual(secret, pem)
    }
}
