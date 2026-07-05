import XCTest
@testable import TsshCore

/// Phase 1D: `ProfileListModel`(Android版`ProfileListViewModel`のMVP部分)の検証。
/// Keychainには触れないため、素の`TsshCoreTests`(非ホスト)で実行できる。
@MainActor
final class ProfileListModelTests: XCTestCase {
    private func makeModel() throws -> (ProfileListModel, ProfileDatabase) {
        let db = try ProfileDatabase.inMemory()
        return (ProfileListModel(db: db), db)
    }

    func testLoadReturnsEmptyListInitially() throws {
        let (model, _) = try makeModel()
        model.load()
        XCTAssertTrue(model.profiles.isEmpty)
    }

    func testLoadReflectsInsertedProfiles() throws {
        let (model, db) = try makeModel()
        var profile = ConnectionProfile(displayName: "dev box", host: "127.0.0.1", port: 22, username: "tester")
        try db.insert(profile: &profile)

        model.load()

        XCTAssertEqual(model.profiles.count, 1)
        XCTAssertEqual(model.profiles.first?.displayName, "dev box")
    }

    func testRequestAndDismissPasswordConnect() throws {
        let (model, _) = try makeModel()
        let profile = ConnectionProfile(displayName: "p", host: "h", port: 22, username: "u")

        model.requestPasswordConnect(profile)
        XCTAssertEqual(model.passwordTarget, profile)

        model.dismissPassword()
        XCTAssertNil(model.passwordTarget)
    }

    func testRequestAndDismissDelete() throws {
        let (model, _) = try makeModel()
        let profile = ConnectionProfile(displayName: "p", host: "h", port: 22, username: "u")

        model.requestDelete(profile)
        XCTAssertEqual(model.deleteTarget, profile)

        model.dismissDelete()
        XCTAssertNil(model.deleteTarget)
    }

    func testConfirmDeleteRemovesProfileAndReloads() throws {
        let (model, db) = try makeModel()
        var profile = ConnectionProfile(displayName: "to delete", host: "h", port: 22, username: "u")
        try db.insert(profile: &profile)
        model.load()
        XCTAssertEqual(model.profiles.count, 1)

        model.requestDelete(profile)
        model.confirmDelete(profile)

        XCTAssertNil(model.deleteTarget)
        XCTAssertTrue(model.profiles.isEmpty)
        XCTAssertTrue(try db.fetchAllProfiles().isEmpty)
    }
}
