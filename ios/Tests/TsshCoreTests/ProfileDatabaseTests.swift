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
        // createdAtはDate()由来だとサブミリ秒精度を持つが、GRDBがSQLiteへ保存する際は
        // ミリ秒精度に丸められるため、Date()のまま厳密等価比較すると丸め誤差で
        // 失敗する。秒単位のDateを明示的に使い、往復での精度損失を避ける。
        let entry = KeyEntry(
            id: "key-1", displayName: "My Key", keyType: "ed25519", publicKey: "AAAA...",
            createdAt: Date(timeIntervalSince1970: 1_700_000_000)
        )

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

    // MARK: - Phase 1E-1: トランスポート/jump host等のフィールド拡張(v2 migration)

    func testNewProfileDefaultsForTransportAndJumpFields() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(displayName: "Server", host: "example.com", port: 22, username: "user")
        try db.insert(profile: &profile)

        let fetched = try XCTUnwrap(try db.fetchProfile(id: profile.id!))
        XCTAssertFalse(fetched.enableAgentForward)
        XCTAssertEqual(fetched.transportPreference, .plainSsh)
        XCTAssertNil(fetched.directAddress)
        XCTAssertFalse(fetched.enablePhysicalMultipath)
        XCTAssertFalse(fetched.enableUpstreamFailover)
        XCTAssertNil(fetched.postConnectCommands)
        XCTAssertEqual(fetched.forwards, [])
        XCTAssertNil(fetched.jumpHost)
        XCTAssertEqual(fetched.jumpPort, 22)
        XCTAssertFalse(fetched.usesJumpHost)
        XCTAssertNil(fetched.stunServer)
        XCTAssertNil(fetched.relayAddr)
        XCTAssertFalse(fetched.allowNonLoopbackForwardBind)
        XCTAssertNil(fetched.themeName)
        XCTAssertNil(fetched.helperBindPort)
    }

    func testProfileWithJumpHostAndForwardsRoundTrips() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(
            displayName: "Via jump",
            host: "internal.example.com",
            port: 22,
            username: "user",
            enableAgentForward: true,
            transportPreference: .isekaiHelperQuicMultipath,
            directAddress: "203.0.113.5:4433",
            forwards: [
                StoredPortForward(kind: .local, bindPort: 8080, remoteHost: "127.0.0.1", remotePort: 80),
                StoredPortForward(kind: .dynamic, bindPort: 1080, remoteHost: "", remotePort: 0),
            ],
            jumpHost: "bastion.example.com",
            jumpPort: 2222,
            jumpUsername: "jumpuser",
            jumpKeyEntryId: "jump-key",
            allowNonLoopbackForwardBind: true,
            themeName: "Dracula",
            helperBindPort: 5555
        )
        try db.insert(profile: &profile)

        let fetched = try XCTUnwrap(try db.fetchProfile(id: profile.id!))
        XCTAssertTrue(fetched.enableAgentForward)
        XCTAssertEqual(fetched.transportPreference, .isekaiHelperQuicMultipath)
        XCTAssertEqual(fetched.directAddress, "203.0.113.5:4433")
        XCTAssertEqual(fetched.forwards.count, 2)
        XCTAssertEqual(fetched.forwards[0].kind, .local)
        XCTAssertEqual(fetched.forwards[0].bindPort, 8080)
        XCTAssertEqual(fetched.forwards[1].kind, .dynamic)
        XCTAssertEqual(fetched.jumpHost, "bastion.example.com")
        XCTAssertEqual(fetched.jumpPort, 2222)
        XCTAssertEqual(fetched.jumpUsername, "jumpuser")
        XCTAssertEqual(fetched.jumpKeyEntryId, "jump-key")
        XCTAssertTrue(fetched.usesJumpHost)
        XCTAssertTrue(fetched.allowNonLoopbackForwardBind)
        XCTAssertEqual(fetched.themeName, "Dracula")
        XCTAssertEqual(fetched.helperBindPort, 5555)
    }

    func testStoredPortForwardConvertsToRealPortForward() {
        let stored = StoredPortForward(kind: .local, bindAddress: "0.0.0.0", bindPort: 8080, remoteHost: "10.0.0.1", remotePort: 443)

        let real = stored.asPortForward

        XCTAssertEqual(real.forwardType, .local)
        XCTAssertEqual(real.bindAddress, "0.0.0.0")
        XCTAssertEqual(real.bindPort, 8080)
        XCTAssertEqual(real.remoteHost, "10.0.0.1")
        XCTAssertEqual(real.remotePort, 443)
    }

    func testStoredTransportPreferenceConvertsToRealTransportPreference() {
        for stored in StoredTransportPreference.allCases {
            // 変換関数が全ケースで例外なく動くこと(網羅性の検証)。
            _ = stored.asTransportPreference
        }
        XCTAssertEqual(StoredTransportPreference.auto.asTransportPreference, .auto)
        XCTAssertEqual(StoredTransportPreference.isekaiStunP2pQuic.asTransportPreference, .isekaiStunP2pQuic)
    }
}
