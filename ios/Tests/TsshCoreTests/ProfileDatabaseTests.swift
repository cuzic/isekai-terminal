import GRDB
import XCTest
@testable import TsshCore

/// Phase 1B: 接続プロファイル管理DB(GRDB)の検証。ファイルI/Oのみで完結し
/// entitlementを必要としないため、素のTsshCoreTestsでそのまま検証できる。
final class ProfileDatabaseTests: XCTestCase {
    func testMigratorAppliesCleanlyToFreshDatabase() throws {
        let db = try ProfileDatabase.inMemory()

        let tableExists = try db.dbQueue.read { conn in
            try conn.tableExists("connection_profile") && conn.tableExists("key_entry")
        }
        XCTAssertTrue(tableExists)
    }

    func testMigratorIsIdempotentWhenAppliedTwice() throws {
        let db = try ProfileDatabase.inMemory()

        // 既に適用済みのDBに対してもう一度migrate()を呼んでも安全であること
        // (Android版room-migration-check相当の考え方、GRDBの標準動作を確認する)。
        XCTAssertNoThrow(try ProfileDatabase.migrator.migrate(db.dbQueue))
    }

    func testInsertAndFetchKeyEntry() throws {
        let db = try ProfileDatabase.inMemory()
        let entry = KeyEntry(id: "key-1", displayName: "My Key", keyType: "ed25519", publicKey: "AAAA...")

        try db.insert(keyEntry: entry)
        let fetched = try db.fetchKeyEntry(id: "key-1")

        XCTAssertEqual(fetched, entry)
    }

    func testInsertAndFetchConnectionProfile() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(displayName: "Home Server", host: "192.168.1.10", port: 22, username: "pi")

        try db.insert(profile: &profile)

        XCTAssertNotNil(profile.id)
        let fetched = try db.fetchProfile(id: profile.id!)
        XCTAssertEqual(fetched?.host, "192.168.1.10")
        XCTAssertEqual(fetched?.username, "pi")
    }

    func testUpdateConnectionProfile() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(displayName: "Server", host: "old.example.com", port: 22, username: "user")
        try db.insert(profile: &profile)

        profile.host = "new.example.com"
        try db.update(profile: profile)

        let fetched = try db.fetchProfile(id: profile.id!)
        XCTAssertEqual(fetched?.host, "new.example.com")
    }

    func testDeleteConnectionProfile() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(displayName: "Temp", host: "temp.example.com", port: 22, username: "user")
        try db.insert(profile: &profile)

        try db.deleteProfile(id: profile.id!)

        XCTAssertNil(try db.fetchProfile(id: profile.id!))
    }

    func testFetchAllProfilesOrderedByDisplayName() throws {
        let db = try ProfileDatabase.inMemory()
        var b = ConnectionProfile(displayName: "B Server", host: "b.example.com", port: 22, username: "user")
        var a = ConnectionProfile(displayName: "A Server", host: "a.example.com", port: 22, username: "user")
        try db.insert(profile: &b)
        try db.insert(profile: &a)

        let all = try db.fetchAllProfiles()

        XCTAssertEqual(all.map(\.displayName), ["A Server", "B Server"])
    }

    func testDeletingKeyEntrySetsProfileKeyEntryIdToNull() throws {
        let db = try ProfileDatabase.inMemory()
        let entry = KeyEntry(id: "key-1", displayName: "My Key", keyType: "ed25519", publicKey: "AAAA...")
        try db.insert(keyEntry: entry)
        var profile = ConnectionProfile(displayName: "Server", host: "example.com", port: 22, username: "user", keyEntryId: "key-1")
        try db.insert(profile: &profile)

        try db.deleteKeyEntry(id: "key-1")

        let fetched = try db.fetchProfile(id: profile.id!)
        XCTAssertNil(fetched?.keyEntryId)
    }
}
