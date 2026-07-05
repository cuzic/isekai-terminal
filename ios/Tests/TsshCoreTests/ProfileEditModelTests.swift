import XCTest
@testable import TsshCore

/// Phase 1D: `ProfileEditModel`(Android版`ProfileEditScreen.kt`のMVP部分)の検証。
/// Keychainには触れないため、素の`TsshCoreTests`(非ホスト)で実行できる。
@MainActor
final class ProfileEditModelTests: XCTestCase {
    func testSaveFailsWithEmptyDisplayName() throws {
        let db = try ProfileDatabase.inMemory()
        let model = ProfileEditModel(profile: nil, db: db)
        model.host = "127.0.0.1"
        model.username = "tester"

        XCTAssertFalse(model.save())
        XCTAssertNotNil(model.errorMessage)
        XCTAssertTrue(try db.fetchAllProfiles().isEmpty)
    }

    func testSaveFailsWithInvalidPort() throws {
        let db = try ProfileDatabase.inMemory()
        let model = ProfileEditModel(profile: nil, db: db)
        model.displayName = "dev box"
        model.host = "127.0.0.1"
        model.username = "tester"
        model.port = "not-a-number"

        XCTAssertFalse(model.save())
        XCTAssertNotNil(model.errorMessage)
    }

    func testSaveInsertsNewProfileWithPasswordAuth() throws {
        let db = try ProfileDatabase.inMemory()
        let model = ProfileEditModel(profile: nil, db: db)
        model.displayName = "dev box"
        model.host = "127.0.0.1"
        model.port = "2222"
        model.username = "tester"

        XCTAssertTrue(model.save())

        let saved = try db.fetchAllProfiles()
        XCTAssertEqual(saved.count, 1)
        XCTAssertEqual(saved.first?.displayName, "dev box")
        XCTAssertEqual(saved.first?.port, 2222)
        XCTAssertNil(saved.first?.keyEntryId)
    }

    func testSaveWithKeyAuthRequiresSelectedKey() throws {
        let db = try ProfileDatabase.inMemory()
        let model = ProfileEditModel(profile: nil, db: db)
        model.displayName = "dev box"
        model.host = "127.0.0.1"
        model.username = "tester"
        model.useKeyAuth = true
        model.selectedKeyEntryId = nil

        XCTAssertFalse(model.save())
        XCTAssertNotNil(model.errorMessage)
    }

    func testSaveWithKeyAuthPersistsKeyEntryId() throws {
        let db = try ProfileDatabase.inMemory()
        try db.insert(keyEntry: KeyEntry(id: "key-1", displayName: "my key", keyType: "ed25519", publicKey: "ssh-ed25519 AAAA"))

        let model = ProfileEditModel(profile: nil, db: db)
        model.displayName = "dev box"
        model.host = "127.0.0.1"
        model.username = "tester"
        model.useKeyAuth = true
        model.selectedKeyEntryId = "key-1"

        XCTAssertTrue(model.save())
        XCTAssertEqual(try db.fetchAllProfiles().first?.keyEntryId, "key-1")
    }

    func testUpdateExistingProfilePreservesId() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(displayName: "original", host: "h", port: 22, username: "u")
        try db.insert(profile: &profile)

        let model = ProfileEditModel(profile: profile, db: db)
        model.displayName = "renamed"

        XCTAssertTrue(model.save())

        let all = try db.fetchAllProfiles()
        XCTAssertEqual(all.count, 1)
        XCTAssertEqual(all.first?.id, profile.id)
        XCTAssertEqual(all.first?.displayName, "renamed")
    }

    func testLoadAvailableKeysPopulatesFromDatabase() throws {
        let db = try ProfileDatabase.inMemory()
        try db.insert(keyEntry: KeyEntry(id: "key-1", displayName: "my key", keyType: "ed25519", publicKey: "ssh-ed25519 AAAA"))

        let model = ProfileEditModel(profile: nil, db: db)
        model.loadAvailableKeys()

        XCTAssertEqual(model.availableKeys.count, 1)
        XCTAssertEqual(model.availableKeys.first?.id, "key-1")
    }
}
