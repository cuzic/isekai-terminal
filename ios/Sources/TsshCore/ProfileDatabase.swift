import Foundation
import GRDB

/// Phase 1B: 接続プロファイル管理のローカルDB。GRDBを第一候補として採用した
/// (接続プロファイルは行データでAndroid Roomと概念的に対称、`DatabaseMigrator`で
/// 明示的マイグレーション管理ができるため。ChatGPT外部レビュー2026-07-04、
/// PLAN.md「Phase Y」節参照)。
///
/// DBには秘密鍵本体を保存しない。`KeyEntry`はCredentialVaultの`key_id`と
/// メタデータ(表示名・鍵種別・公開鍵・認証ポリシー)だけを持ち、実際の秘密材料は
/// `CredentialVault`が別途暗号化して管理する。

/// Android版`KeyEntry`相当。`id`は`CredentialVault.Metadata.keyId`と対応する。
public struct KeyEntry: Codable, Equatable, FetchableRecord, PersistableRecord {
    public var id: String
    public var displayName: String
    public var keyType: String
    public var publicKey: String
    /// 将来のSecure Enclave/生体認証必須モード拡張用("standard"が既定)。
    public var authenticationPolicy: String
    public var createdAt: Date

    public static let databaseTableName = "key_entry"

    public init(
        id: String,
        displayName: String,
        keyType: String,
        publicKey: String,
        authenticationPolicy: String = "standard",
        createdAt: Date = Date()
    ) {
        self.id = id
        self.displayName = displayName
        self.keyType = keyType
        self.publicKey = publicKey
        self.authenticationPolicy = authenticationPolicy
        self.createdAt = createdAt
    }
}

/// Android版`ConnectionProfile`相当(MVPスコープ、必要に応じて後続Phaseで拡張)。
public struct ConnectionProfile: Codable, Equatable, FetchableRecord, MutablePersistableRecord {
    public var id: Int64?
    public var displayName: String
    public var host: String
    public var port: Int
    public var username: String
    /// nilならパスワード認証(パスワード自体はDBに保存せずCredentialVaultが管理)。
    public var keyEntryId: String?
    public var createdAt: Date

    public static let databaseTableName = "connection_profile"

    public init(
        id: Int64? = nil,
        displayName: String,
        host: String,
        port: Int,
        username: String,
        keyEntryId: String? = nil,
        createdAt: Date = Date()
    ) {
        self.id = id
        self.displayName = displayName
        self.host = host
        self.port = port
        self.username = username
        self.keyEntryId = keyEntryId
        self.createdAt = createdAt
    }

    public mutating func didInsert(_ inserted: InsertionSuccess) {
        id = inserted.rowID
    }
}

public final class ProfileDatabase {
    public let dbQueue: DatabaseQueue

    public init(path: String) throws {
        dbQueue = try DatabaseQueue(path: path)
        try Self.migrator.migrate(dbQueue)
    }

    /// テスト用のインメモリDB。
    public static func inMemory() throws -> ProfileDatabase {
        try ProfileDatabase(path: ":memory:")
    }

    /// Android版`room-migration-check`相当の考え方: マイグレーションは
    /// 登録順に一度だけ適用され、既に適用済みのDBに対して再度`migrate()`を
    /// 呼んでも安全(冪等)であることをテストで確認する(GRDBの標準動作)。
    static var migrator: DatabaseMigrator {
        var migrator = DatabaseMigrator()
        migrator.registerMigration("v1_create_key_entry_and_connection_profile") { db in
            try db.create(table: "key_entry") { t in
                t.column("id", .text).primaryKey()
                t.column("displayName", .text).notNull()
                t.column("keyType", .text).notNull()
                t.column("publicKey", .text).notNull()
                t.column("authenticationPolicy", .text).notNull()
                t.column("createdAt", .datetime).notNull()
            }
            try db.create(table: "connection_profile") { t in
                t.autoIncrementedPrimaryKey("id")
                t.column("displayName", .text).notNull()
                t.column("host", .text).notNull()
                t.column("port", .integer).notNull()
                t.column("username", .text).notNull()
                t.column("keyEntryId", .text)
                    .references("key_entry", onDelete: .setNull)
                t.column("createdAt", .datetime).notNull()
            }
        }
        return migrator
    }

    // MARK: - KeyEntry CRUD

    public func insert(keyEntry: KeyEntry) throws {
        try dbQueue.write { db in try keyEntry.insert(db) }
    }

    public func fetchAllKeyEntries() throws -> [KeyEntry] {
        try dbQueue.read { db in try KeyEntry.order(Column("displayName")).fetchAll(db) }
    }

    public func fetchKeyEntry(id: String) throws -> KeyEntry? {
        try dbQueue.read { db in try KeyEntry.fetchOne(db, key: id) }
    }

    public func deleteKeyEntry(id: String) throws {
        _ = try dbQueue.write { db in try KeyEntry.deleteOne(db, key: id) }
    }

    // MARK: - ConnectionProfile CRUD

    public func insert(profile: inout ConnectionProfile) throws {
        try dbQueue.write { db in try profile.insert(db) }
    }

    public func update(profile: ConnectionProfile) throws {
        try dbQueue.write { db in try profile.update(db) }
    }

    public func deleteProfile(id: Int64) throws {
        _ = try dbQueue.write { db in try ConnectionProfile.deleteOne(db, key: id) }
    }

    public func fetchAllProfiles() throws -> [ConnectionProfile] {
        try dbQueue.read { db in try ConnectionProfile.order(Column("displayName")).fetchAll(db) }
    }

    public func fetchProfile(id: Int64) throws -> ConnectionProfile? {
        try dbQueue.read { db in try ConnectionProfile.fetchOne(db, key: id) }
    }
}
