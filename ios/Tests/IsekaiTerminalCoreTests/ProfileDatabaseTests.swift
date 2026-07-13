import GRDB
import XCTest
@testable import IsekaiTerminalCore

/// Phase 1B: 接続プロファイル管理DB(GRDB)の検証。ファイルI/Oのみで完結し
/// entitlementを必要としないため、素のIsekaiTerminalCoreTestsでそのまま検証できる。
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

    /// `StoredTransportPreference`が`DatabaseValueConvertible`により素の文字列として
    /// カラムへ保存されること(JSON文字列として二重にラップされていないこと)を
    /// 生カラム値を直接読んで確認する。v2 migrationの`ALTER TABLE`デフォルト値
    /// (素の文字列リテラル)と表現が一致している必要があるため。
    func testTransportPreferenceIsStoredAsPlainStringNotJson() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(
            displayName: "test", host: "h", port: 22, username: "u",
            transportPreference: .isekaiStunP2pQuic
        )
        try db.insert(profile: &profile)

        let rawValue: String? = try db.dbQueue.read { conn in
            try String.fetchOne(conn, sql: "SELECT transportPreference FROM connection_profile WHERE id = ?", arguments: [profile.id!])
        }

        XCTAssertEqual(rawValue, "isekaiStunP2pQuic")
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

    // MARK: - Phase 1G-1(#53): Snippet CRUD

    func testInsertAndFetchSnippet() throws {
        let db = try ProfileDatabase.inMemory()
        var snippet = Snippet(label: "list files", command: "ls -la")

        try db.insert(snippet: &snippet)

        XCTAssertNotNil(snippet.id)
        let fetched = try db.fetchSnippet(id: snippet.id!)
        XCTAssertEqual(fetched?.label, "list files")
        XCTAssertEqual(fetched?.command, "ls -la")
        XCTAssertTrue(fetched?.appendNewline ?? false)
        XCTAssertNil(fetched?.profileId)
    }

    func testUpdateSnippet() throws {
        let db = try ProfileDatabase.inMemory()
        var snippet = Snippet(label: "old", command: "echo old")
        try db.insert(snippet: &snippet)

        snippet.label = "new"
        snippet.command = "echo new"
        try db.update(snippet: snippet)

        let fetched = try db.fetchSnippet(id: snippet.id!)
        XCTAssertEqual(fetched?.label, "new")
        XCTAssertEqual(fetched?.command, "echo new")
    }

    func testDeleteSnippet() throws {
        let db = try ProfileDatabase.inMemory()
        var snippet = Snippet(label: "temp", command: "echo temp")
        try db.insert(snippet: &snippet)

        try db.deleteSnippet(id: snippet.id!)

        XCTAssertNil(try db.fetchSnippet(id: snippet.id!))
    }

    func testFetchAllSnippetsOrderedBySortOrderThenLabel() throws {
        let db = try ProfileDatabase.inMemory()
        var b = Snippet(label: "B", command: "echo b", sortOrder: 1)
        var a = Snippet(label: "A", command: "echo a", sortOrder: 0)
        var c = Snippet(label: "C", command: "echo c", sortOrder: 1)
        try db.insert(snippet: &b)
        try db.insert(snippet: &a)
        try db.insert(snippet: &c)

        let all = try db.fetchAllSnippets()

        XCTAssertEqual(all.map(\.label), ["A", "B", "C"])
    }

    func testSnippetWithProfileIdRoundTrips() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        try db.insert(profile: &profile)
        var snippet = Snippet(label: "profile-specific", command: "echo hi", profileId: profile.id)

        try db.insert(snippet: &snippet)

        let fetched = try db.fetchSnippet(id: snippet.id!)
        XCTAssertEqual(fetched?.profileId, profile.id)
    }

    func testFetchSnippetsForProfileIncludesSharedAndProfileSpecific() throws {
        let db = try ProfileDatabase.inMemory()
        var profileA = ConnectionProfile(displayName: "A", host: "a.example.com", port: 22, username: "user")
        var profileB = ConnectionProfile(displayName: "B", host: "b.example.com", port: 22, username: "user")
        try db.insert(profile: &profileA)
        try db.insert(profile: &profileB)
        var shared = Snippet(label: "shared", command: "echo shared")
        var forA = Snippet(label: "for-a", command: "echo a", profileId: profileA.id)
        var forB = Snippet(label: "for-b", command: "echo b", profileId: profileB.id)
        try db.insert(snippet: &shared)
        try db.insert(snippet: &forA)
        try db.insert(snippet: &forB)

        let forProfileA = try db.fetchSnippets(forProfileId: profileA.id)

        XCTAssertEqual(Set(forProfileA.map(\.label)), Set(["shared", "for-a"]))
    }

    func testFetchSnippetsForNilProfileReturnsOnlySharedSnippets() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        try db.insert(profile: &profile)
        var shared = Snippet(label: "shared", command: "echo shared")
        var specific = Snippet(label: "specific", command: "echo specific", profileId: profile.id)
        try db.insert(snippet: &shared)
        try db.insert(snippet: &specific)

        let result = try db.fetchSnippets(forProfileId: nil)

        XCTAssertEqual(result.map(\.label), ["shared"])
    }

    // MARK: - 打鍵列(KeySequence) CRUD

    func testInsertAndFetchKeySequence() throws {
        let db = try ProfileDatabase.inMemory()
        var keySequence = KeySequence.create(label: "tmux new window", steps: [.ctrlChar("b"), .text("c")])

        try db.insert(keySequence: &keySequence)

        XCTAssertNotNil(keySequence.id)
        let fetched = try db.fetchKeySequence(id: keySequence.id!)
        XCTAssertEqual(fetched?.label, "tmux new window")
        XCTAssertEqual(fetched?.steps, [.ctrlChar("b"), .text("c")])
        XCTAssertNil(fetched?.profileId)
    }

    func testUpdateKeySequence() throws {
        let db = try ProfileDatabase.inMemory()
        var keySequence = KeySequence.create(label: "old", steps: [.text("old")])
        try db.insert(keySequence: &keySequence)

        var updated = keySequence
        updated.label = "new"
        updated.stepsJson = KeyStepJSON.encode([.text("new")])
        try db.update(keySequence: updated)

        let fetched = try db.fetchKeySequence(id: keySequence.id!)
        XCTAssertEqual(fetched?.label, "new")
        XCTAssertEqual(fetched?.steps, [.text("new")])
    }

    func testDeleteKeySequence() throws {
        let db = try ProfileDatabase.inMemory()
        var keySequence = KeySequence.create(label: "temp", steps: [.text("temp")])
        try db.insert(keySequence: &keySequence)

        try db.deleteKeySequence(id: keySequence.id!)

        XCTAssertNil(try db.fetchKeySequence(id: keySequence.id!))
    }

    func testFetchAllKeySequencesOrderedBySortOrderThenLabel() throws {
        let db = try ProfileDatabase.inMemory()
        var b = KeySequence.create(label: "B", steps: [.text("b")], sortOrder: 1)
        var a = KeySequence.create(label: "A", steps: [.text("a")], sortOrder: 0)
        var c = KeySequence.create(label: "C", steps: [.text("c")], sortOrder: 1)
        try db.insert(keySequence: &b)
        try db.insert(keySequence: &a)
        try db.insert(keySequence: &c)

        let all = try db.fetchAllKeySequences()

        XCTAssertEqual(all.map(\.label), ["A", "B", "C"])
    }

    func testKeySequenceWithProfileIdRoundTrips() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        try db.insert(profile: &profile)
        var keySequence = KeySequence.create(label: "profile-specific", steps: [.text("hi")], profileId: profile.id)

        try db.insert(keySequence: &keySequence)

        let fetched = try db.fetchKeySequence(id: keySequence.id!)
        XCTAssertEqual(fetched?.profileId, profile.id)
    }

    func testFetchKeySequencesForProfileIncludesSharedAndProfileSpecific() throws {
        let db = try ProfileDatabase.inMemory()
        var profileA = ConnectionProfile(displayName: "A", host: "a.example.com", port: 22, username: "user")
        var profileB = ConnectionProfile(displayName: "B", host: "b.example.com", port: 22, username: "user")
        try db.insert(profile: &profileA)
        try db.insert(profile: &profileB)
        var shared = KeySequence.create(label: "shared", steps: [.text("shared")])
        var forA = KeySequence.create(label: "for-a", steps: [.text("a")], profileId: profileA.id)
        var forB = KeySequence.create(label: "for-b", steps: [.text("b")], profileId: profileB.id)
        try db.insert(keySequence: &shared)
        try db.insert(keySequence: &forA)
        try db.insert(keySequence: &forB)

        let forProfileA = try db.fetchKeySequences(forProfileId: profileA.id)

        XCTAssertEqual(Set(forProfileA.map(\.label)), Set(["shared", "for-a"]))
    }

    func testFetchKeySequencesForNilProfileReturnsOnlySharedKeySequences() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        try db.insert(profile: &profile)
        var shared = KeySequence.create(label: "shared", steps: [.text("shared")])
        var specific = KeySequence.create(label: "specific", steps: [.text("specific")], profileId: profile.id)
        try db.insert(keySequence: &shared)
        try db.insert(keySequence: &specific)

        let result = try db.fetchKeySequences(forProfileId: nil)

        XCTAssertEqual(result.map(\.label), ["shared"])
    }

    func testCorruptedStepsJsonDoesNotThrow_stepsFallsBackToEmpty() throws {
        let db = try ProfileDatabase.inMemory()
        var keySequence = KeySequence(label: "broken", stepsJson: "{not valid json")
        try db.insert(keySequence: &keySequence)

        let fetched = try db.fetchKeySequence(id: keySequence.id!)
        XCTAssertEqual(fetched?.steps, [])
    }

    func testKeySequenceCommandsToBytesOverloadDelegatesToStepsBasedOverload() throws {
        let db = try ProfileDatabase.inMemory()
        var keySequence = KeySequence.create(label: "tmux new window", steps: [.ctrlChar("b"), .text("c")])
        try db.insert(keySequence: &keySequence)
        let fetched = try db.fetchKeySequence(id: keySequence.id!)!

        let bytes = KeySequenceCommands.toBytes(keySequence: fetched)

        XCTAssertEqual(bytes, Data([0x02, UInt8(ascii: "c")]))
    }

    // MARK: - 打鍵列セット(パック)インストール状態

    func testInstallPackThenFetchGlobalReturnsInstallation() throws {
        let db = try ProfileDatabase.inMemory()
        try db.installPack(packId: "tmux", version: 1, paramValues: ["prefix": .ctrlChar("b")])

        let found = try db.fetchGlobalPackInstallation(packId: "tmux")
        XCTAssertEqual(found?.packId, "tmux")
        XCTAssertEqual(found?.paramValues, ["prefix": .ctrlChar("b")])
    }

    func testInstallPackCalledTwiceForSameGlobalPackReplacesRatherThanDuplicates() throws {
        let db = try ProfileDatabase.inMemory()
        try db.installPack(packId: "tmux", version: 1, paramValues: ["prefix": .ctrlChar("b")])
        try db.installPack(packId: "tmux", version: 1, paramValues: ["prefix": .ctrlChar("a")])

        let all = try db.dbQueue.read { db in try KeySequencePackInstallation.fetchAll(db) }
        XCTAssertEqual(all.count, 1)
        XCTAssertEqual(all.first?.paramValues, ["prefix": .ctrlChar("a")])
    }

    func testResolvePackInstallationProfileSpecificTakesPriorityOverGlobal() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(displayName: "web", host: "h", port: 22, username: "u")
        try db.insert(profile: &profile)
        try db.installPack(packId: "tmux", version: 1, paramValues: ["prefix": .ctrlChar("b")])
        try db.installPack(packId: "tmux", version: 1, paramValues: ["prefix": .ctrlChar("a")], profileId: profile.id)

        let resolved = try db.resolvePackInstallation(packId: "tmux", profileId: profile.id)
        XCTAssertEqual(resolved?.paramValues, ["prefix": .ctrlChar("a")])
    }

    func testResolvePackInstallationFallsBackToGlobalWhenNoProfileSpecificInstallation() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(displayName: "web", host: "h", port: 22, username: "u")
        try db.insert(profile: &profile)
        try db.installPack(packId: "tmux", version: 1, paramValues: ["prefix": .ctrlChar("b")])

        let resolved = try db.resolvePackInstallation(packId: "tmux", profileId: profile.id)
        XCTAssertEqual(resolved?.paramValues, ["prefix": .ctrlChar("b")])
    }

    func testResolvePackInstallationReturnsNilWhenNotInstalledAnywhere() throws {
        let db = try ProfileDatabase.inMemory()
        XCTAssertNil(try db.resolvePackInstallation(packId: "tmux", profileId: nil))
    }

    func testDeletePackInstallationRemovesInstallation() throws {
        let db = try ProfileDatabase.inMemory()
        try db.installPack(packId: "tmux", version: 1, paramValues: [:])
        let installed = try db.fetchGlobalPackInstallation(packId: "tmux")!

        try db.deletePackInstallation(id: installed.id!)

        XCTAssertNil(try db.fetchGlobalPackInstallation(packId: "tmux"))
    }

    func testInstallPackCalledConcurrentlyDoesNotCreateDuplicateGlobalRows() async throws {
        // GRDBのDatabaseQueueはwriteを直列化するため、installPack内の「検索してから書き込む」
        // トランザクションが同時に複数呼ばれても重複行を作らないことを確認する
        // (Android版のMutexで防いでいるのと同じ問題への、GRDBに適した解決の検証)。
        let db = try ProfileDatabase.inMemory()

        try await withThrowingTaskGroup(of: Void.self) { group in
            for i in 0..<20 {
                group.addTask {
                    let c = Character(UnicodeScalar(UInt8(ascii: "a") + UInt8(i % 26)))
                    try db.installPack(packId: "tmux", version: 1, paramValues: ["prefix": .ctrlChar(c)])
                }
            }
            try await group.waitForAll()
        }

        let all = try db.dbQueue.read { db in try KeySequencePackInstallation.fetchAll(db) }
        XCTAssertEqual(all.count, 1)
    }
}
