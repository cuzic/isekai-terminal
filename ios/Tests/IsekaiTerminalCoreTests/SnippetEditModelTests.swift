import XCTest
@testable import IsekaiTerminalCore

/// Phase 1G-1(#53): `SnippetEditModel`(Android版`SnippetEditViewModel`相当)の検証。
@MainActor
final class SnippetEditModelTests: XCTestCase {
    func testSaveFailsWithEmptyLabel() throws {
        let db = try ProfileDatabase.inMemory()
        let model = SnippetEditModel(snippet: nil, db: db)
        model.command = "ls -la"

        XCTAssertFalse(model.save())
        XCTAssertNotNil(model.errorMessage)
        XCTAssertTrue(try db.fetchAllSnippets().isEmpty)
    }

    func testSaveFailsWithEmptyCommand() throws {
        let db = try ProfileDatabase.inMemory()
        let model = SnippetEditModel(snippet: nil, db: db)
        model.label = "list files"

        XCTAssertFalse(model.save())
        XCTAssertNotNil(model.errorMessage)
    }

    func testSaveInsertsNewSnippet() throws {
        let db = try ProfileDatabase.inMemory()
        let model = SnippetEditModel(snippet: nil, db: db)
        model.label = "list files"
        model.command = "ls -la"

        XCTAssertTrue(model.save())

        let saved = try db.fetchAllSnippets()
        XCTAssertEqual(saved.count, 1)
        XCTAssertEqual(saved.first?.label, "list files")
        XCTAssertEqual(saved.first?.command, "ls -la")
        XCTAssertTrue(saved.first?.appendNewline ?? false)
        XCTAssertNil(saved.first?.profileId)
    }

    func testSaveWithProfileIdPersistsScope() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        try db.insert(profile: &profile)
        let model = SnippetEditModel(snippet: nil, db: db)
        model.label = "scoped"
        model.command = "echo hi"
        model.profileId = profile.id

        XCTAssertTrue(model.save())

        let saved = try db.fetchAllSnippets().first
        XCTAssertEqual(saved?.profileId, profile.id)
    }

    func testUpdateExistingSnippetPreservesId() throws {
        let db = try ProfileDatabase.inMemory()
        var snippet = Snippet(label: "original", command: "echo original")
        try db.insert(snippet: &snippet)

        let model = SnippetEditModel(snippet: snippet, db: db)
        model.label = "renamed"

        XCTAssertTrue(model.save())

        let all = try db.fetchAllSnippets()
        XCTAssertEqual(all.count, 1)
        XCTAssertEqual(all.first?.id, snippet.id)
        XCTAssertEqual(all.first?.label, "renamed")
    }

    func testLoadAvailableProfilesPopulatesFromDatabase() throws {
        let db = try ProfileDatabase.inMemory()
        var profile = ConnectionProfile(displayName: "dev box", host: "127.0.0.1", port: 22, username: "tester")
        try db.insert(profile: &profile)

        let model = SnippetEditModel(snippet: nil, db: db)
        model.loadAvailableProfiles()

        XCTAssertEqual(model.availableProfiles.count, 1)
        XCTAssertEqual(model.availableProfiles.first?.id, profile.id)
    }

    func testEditingExistingSnippetRestoresFields() throws {
        let db = try ProfileDatabase.inMemory()
        var snippet = Snippet(label: "existing", command: "echo hi", profileId: nil, appendNewline: false)
        try db.insert(snippet: &snippet)

        let model = SnippetEditModel(snippet: snippet, db: db)

        XCTAssertEqual(model.label, "existing")
        XCTAssertEqual(model.command, "echo hi")
        XCTAssertFalse(model.appendNewline)
    }
}
