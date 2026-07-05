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

    // MARK: - Phase 1E-2: 踏み台(ProxyJump)

    func testSaveWithJumpHostRequiresJumpFields() throws {
        let db = try ProfileDatabase.inMemory()
        let model = ProfileEditModel(profile: nil, db: db)
        model.displayName = "dev box"
        model.host = "127.0.0.1"
        model.username = "tester"
        model.useJumpHost = true
        model.jumpHost = ""

        XCTAssertFalse(model.save())
        XCTAssertNotNil(model.errorMessage)
    }

    func testSaveWithJumpHostPersistsJumpFields() throws {
        let db = try ProfileDatabase.inMemory()
        let model = ProfileEditModel(profile: nil, db: db)
        model.displayName = "dev box"
        model.host = "internal.example.com"
        model.username = "tester"
        model.useJumpHost = true
        model.jumpHost = "bastion.example.com"
        model.jumpPort = "2222"
        model.jumpUsername = "jumpuser"

        XCTAssertTrue(model.save())

        let saved = try XCTUnwrap(try db.fetchAllProfiles().first)
        XCTAssertEqual(saved.jumpHost, "bastion.example.com")
        XCTAssertEqual(saved.jumpPort, 2222)
        XCTAssertEqual(saved.jumpUsername, "jumpuser")
        XCTAssertNil(saved.jumpKeyEntryId)
        XCTAssertTrue(saved.usesJumpHost)
    }

    func testSaveWithJumpKeyAuthRequiresSelectedKey() throws {
        let db = try ProfileDatabase.inMemory()
        let model = ProfileEditModel(profile: nil, db: db)
        model.displayName = "dev box"
        model.host = "internal.example.com"
        model.username = "tester"
        model.useJumpHost = true
        model.jumpHost = "bastion.example.com"
        model.jumpUsername = "jumpuser"
        model.jumpUseKeyAuth = true
        model.jumpSelectedKeyEntryId = nil

        XCTAssertFalse(model.save())
        XCTAssertNotNil(model.errorMessage)
    }

    func testDisablingJumpHostClearsJumpFieldsOnSave() throws {
        let db = try ProfileDatabase.inMemory()
        let model = ProfileEditModel(profile: nil, db: db)
        model.displayName = "dev box"
        model.host = "127.0.0.1"
        model.username = "tester"
        model.useJumpHost = false
        model.jumpHost = "leftover.example.com" // 入力欄に残っていてもuseJumpHost=falseなら無視される

        XCTAssertTrue(model.save())

        let saved = try XCTUnwrap(try db.fetchAllProfiles().first)
        XCTAssertNil(saved.jumpHost)
        XCTAssertFalse(saved.usesJumpHost)
    }

    // MARK: - Phase 1E-3: ポートフォワード

    func testAddAndRemoveForward() {
        let model = ProfileEditModel(profile: nil, db: try! ProfileDatabase.inMemory())
        let forward = StoredPortForward(kind: .local, bindPort: 8080, remoteHost: "127.0.0.1", remotePort: 80)

        model.addForward(forward)
        XCTAssertEqual(model.forwards, [forward])

        model.removeForward(at: IndexSet(integer: 0))
        XCTAssertTrue(model.forwards.isEmpty)
    }

    func testSavePersistsForwards() throws {
        let db = try ProfileDatabase.inMemory()
        let model = ProfileEditModel(profile: nil, db: db)
        model.displayName = "dev box"
        model.host = "127.0.0.1"
        model.username = "tester"
        model.addForward(StoredPortForward(kind: .local, bindPort: 8080, remoteHost: "127.0.0.1", remotePort: 80))
        model.allowNonLoopbackForwardBind = true

        XCTAssertTrue(model.save())

        let saved = try XCTUnwrap(try db.fetchAllProfiles().first)
        XCTAssertEqual(saved.forwards.count, 1)
        XCTAssertEqual(saved.forwards.first?.bindPort, 8080)
        XCTAssertTrue(saved.allowNonLoopbackForwardBind)
    }

    // MARK: - Phase 1E-4: SSH agent forwarding

    func testSavePersistsEnableAgentForward() throws {
        let db = try ProfileDatabase.inMemory()
        let model = ProfileEditModel(profile: nil, db: db)
        model.displayName = "dev box"
        model.host = "127.0.0.1"
        model.username = "tester"
        model.enableAgentForward = true

        XCTAssertTrue(model.save())

        let saved = try XCTUnwrap(try db.fetchAllProfiles().first)
        XCTAssertTrue(saved.enableAgentForward)
    }

    // MARK: - 既存プロファイルの編集時の初期値復元

    func testEditingExistingProfileRestoresJumpAndForwardFields() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(
            displayName: "existing",
            host: "internal.example.com",
            port: 22,
            username: "user",
            enableAgentForward: true,
            forwards: [StoredPortForward(kind: .remote, bindPort: 9090, remoteHost: "127.0.0.1", remotePort: 3000)],
            jumpHost: "bastion.example.com",
            jumpPort: 2200,
            jumpUsername: "jumpuser"
        )
        try db.insert(profile: &profile)

        let model = ProfileEditModel(profile: profile, db: db)

        XCTAssertTrue(model.useJumpHost)
        XCTAssertEqual(model.jumpHost, "bastion.example.com")
        XCTAssertEqual(model.jumpPort, "2200")
        XCTAssertEqual(model.jumpUsername, "jumpuser")
        XCTAssertEqual(model.forwards.count, 1)
        XCTAssertTrue(model.enableAgentForward)
    }
}
