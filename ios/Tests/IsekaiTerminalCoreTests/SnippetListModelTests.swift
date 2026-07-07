import XCTest
@testable import IsekaiTerminalCore

/// Phase 1G-1(#53): `SnippetListModel`(Android版`SnippetListViewModel`相当)の検証。
/// Keychainには触れないため、素の`IsekaiTerminalCoreTests`(非ホスト)で実行できる。
@MainActor
final class SnippetListModelTests: XCTestCase {
    private func makeModel() throws -> (SnippetListModel, ProfileDatabase) {
        let db = try ProfileDatabase.inMemory()
        return (SnippetListModel(db: db), db)
    }

    func testLoadReturnsEmptyListInitially() throws {
        let (model, _) = try makeModel()
        model.load()
        XCTAssertTrue(model.snippets.isEmpty)
    }

    func testLoadReflectsInsertedSnippets() throws {
        let (model, db) = try makeModel()
        var snippet = Snippet(label: "list files", command: "ls -la")
        try db.insert(snippet: &snippet)

        model.load()

        XCTAssertEqual(model.snippets.count, 1)
        XCTAssertEqual(model.snippets.first?.label, "list files")
    }

    func testRequestAndDismissDelete() throws {
        let (model, _) = try makeModel()
        let snippet = Snippet(id: 1, label: "s", command: "echo s")

        model.requestDelete(snippet)
        XCTAssertEqual(model.deleteTarget, snippet)

        model.dismissDelete()
        XCTAssertNil(model.deleteTarget)
    }

    func testConfirmDeleteRemovesSnippetAndReloads() throws {
        let (model, db) = try makeModel()
        var snippet = Snippet(label: "to delete", command: "echo bye")
        try db.insert(snippet: &snippet)
        model.load()
        XCTAssertEqual(model.snippets.count, 1)

        model.requestDelete(snippet)
        model.confirmDelete(snippet)

        XCTAssertNil(model.deleteTarget)
        XCTAssertTrue(model.snippets.isEmpty)
        XCTAssertTrue(try db.fetchAllSnippets().isEmpty)
    }
}
