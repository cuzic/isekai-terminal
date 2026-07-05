import Foundation
import GRDB
import TsshCoreLogic

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

/// Android版`PortForward`(uniffi生成のRust型)の保存用の軽量な複製。
/// `generated/tssh_core.swift`の`PortForward`/`ForwardType`は`Codable`に対応して
/// いない(別ファイルの型へextensionで`Codable`適合を後付けしてもSwiftは自動合成しない
/// ため)。GRDBのJSON列として保存できるようこの専用型を使い、実際に接続する際
/// (Phase 1E-3)に`asPortForward`でRust側の`PortForward`へ変換する。
public struct StoredPortForward: Codable, Equatable, Hashable {
    public enum Kind: String, Codable, Equatable, Hashable, CaseIterable {
        case local, remote, dynamic
    }

    public var kind: Kind
    public var bindAddress: String
    public var bindPort: UInt16
    public var remoteHost: String
    public var remotePort: UInt16

    public init(kind: Kind, bindAddress: String = "127.0.0.1", bindPort: UInt16, remoteHost: String, remotePort: UInt16) {
        self.kind = kind
        self.bindAddress = bindAddress
        self.bindPort = bindPort
        self.remoteHost = remoteHost
        self.remotePort = remotePort
    }

    public var asPortForward: PortForward {
        let forwardType: ForwardType
        switch kind {
        case .local: forwardType = .local
        case .remote: forwardType = .remote
        case .dynamic: forwardType = .dynamic
        }
        return PortForward(forwardType: forwardType, bindAddress: bindAddress, bindPort: bindPort, remoteHost: remoteHost, remotePort: remotePort)
    }
}

/// Android版`TransportPreference`の保存用の軽量な複製(理由は`StoredPortForward`と同じ)。
/// DBには`rawValue`をTEXTとして保存し、`asTransportPreference`でRust側の実際の
/// `TransportPreference`へ変換する。`DatabaseValueConvertible`に明示適合させることで、
/// GRDBのCodableレコード機構が(JSON文字列として二重にラップせず)素の文字列として
/// 直接カラムへ読み書きするようにする(v2 migrationの`ALTER TABLE`デフォルト値が
/// 素の文字列リテラルであることと一致させるため)。
public enum StoredTransportPreference: String, Codable, Equatable, Hashable, CaseIterable, DatabaseValueConvertible {
    case plainSsh
    case tsshdQuic
    case isekaiHelperQuic
    case auto
    case isekaiHelperQuicMultipath
    case isekaiStunP2pQuic
    case isekaiLinkRelayQuic

    public var asTransportPreference: TransportPreference {
        switch self {
        case .plainSsh: return .plainSsh
        case .tsshdQuic: return .tsshdQuic
        case .isekaiHelperQuic: return .isekaiHelperQuic
        case .auto: return .auto
        case .isekaiHelperQuicMultipath: return .isekaiHelperQuicMultipath
        case .isekaiStunP2pQuic: return .isekaiStunP2pQuic
        case .isekaiLinkRelayQuic: return .isekaiLinkRelayQuic
        }
    }
}

/// Android版`ConnectionProfile`相当。Phase 1E(トランスポート/接続方式パリティ)で
/// jump host・トランスポート方式・ポートフォワード・agent forward等のフィールドを追加した。
/// `Hashable`はSwiftUIの`NavigationStack`パス(`AppRoute`)に格納するために必要。
public struct ConnectionProfile: Codable, Equatable, Hashable, FetchableRecord, MutablePersistableRecord {
    public var id: Int64?
    public var displayName: String
    public var host: String
    public var port: Int
    public var username: String
    /// nilならパスワード認証(パスワード自体はDBに保存せずCredentialVaultが管理)。
    public var keyEntryId: String?
    public var createdAt: Date

    /// SSH agent forwarding。既定OFF・プロファイル単位opt-in(Android版と同じ方針)。
    public var enableAgentForward: Bool
    /// トランスポート戦略。DBには`StoredTransportPreference.rawValue`を保存する。
    public var transportPreference: StoredTransportPreference
    /// マルチパス(path1)用の直接到達アドレス。`isekaiHelperQuicMultipath`選択時のみ使う。
    public var directAddress: String?
    /// 実験的機能・既定OFF: Wi-Fi/セルラー物理無線への同時マルチパスも試す。
    public var enablePhysicalMultipath: Bool
    /// 実験的機能: セルラー物理path候補だけdirectAddressとは別のリモートアドレスへ向ける。
    public var cellularRemoteAddress: String?
    /// 実験的機能・既定OFF: upstream失効検知時にセルラーへ丸ごと切り替える。
    public var enableUpstreamFailover: Bool
    /// 接続確立後に自動実行するコマンド列(改行区切り、複数可)。nil/空なら何もしない。
    public var postConnectCommands: String?
    /// ローカル/リモート/ダイナミックポートフォワード一覧。GRDBのCodable JSON列として保存。
    public var forwards: [StoredPortForward]
    /// 多段SSH(ProxyJump)。nilなら直接接続。
    public var jumpHost: String?
    public var jumpPort: Int
    public var jumpUsername: String?
    /// nilならパスワード認証(踏み台自身の認証方式)。
    public var jumpKeyEntryId: String?
    /// STUN+SSHランデブーP2P(`isekaiStunP2pQuic`)選択時のみ使うSTUNサーバー(host:port)。
    public var stunServer: String?
    /// MASQUE relay経由P2P(`isekaiLinkRelayQuic`)選択時のみ使う。`relayJwt`は平文ではなく
    /// CredentialVault経由で暗号化した値を保存すること(Phase 1E-6で対応、現時点ではまだ
    /// 平文格納のプレースホルダー)。
    public var relayAddr: String?
    public var relaySni: String?
    public var relayJwt: String?
    /// ポートフォワードの非ループバックbindを明示許可するか。既定false。
    public var allowNonLoopbackForwardBind: Bool
    /// プロファイル単位の配色テーマ既定(`TerminalThemes`のプリセット名)。nilならグローバル既定。
    public var themeName: String?
    /// isekai-helper QUICの待受ポートを固定する(nil=OSがエフェメラルポートを選ぶ)。
    public var helperBindPort: Int?

    public static let databaseTableName = "connection_profile"

    /// 踏み台ホストが設定されているか(多段SSHを使うプロファイルか)。
    public var usesJumpHost: Bool {
        !(jumpHost?.isEmpty ?? true)
    }

    public init(
        id: Int64? = nil,
        displayName: String,
        host: String,
        port: Int,
        username: String,
        keyEntryId: String? = nil,
        createdAt: Date = Date(),
        enableAgentForward: Bool = false,
        transportPreference: StoredTransportPreference = .plainSsh,
        directAddress: String? = nil,
        enablePhysicalMultipath: Bool = false,
        cellularRemoteAddress: String? = nil,
        enableUpstreamFailover: Bool = false,
        postConnectCommands: String? = nil,
        forwards: [StoredPortForward] = [],
        jumpHost: String? = nil,
        jumpPort: Int = 22,
        jumpUsername: String? = nil,
        jumpKeyEntryId: String? = nil,
        stunServer: String? = nil,
        relayAddr: String? = nil,
        relaySni: String? = nil,
        relayJwt: String? = nil,
        allowNonLoopbackForwardBind: Bool = false,
        themeName: String? = nil,
        helperBindPort: Int? = nil
    ) {
        self.id = id
        self.displayName = displayName
        self.host = host
        self.port = port
        self.username = username
        self.keyEntryId = keyEntryId
        self.createdAt = createdAt
        self.enableAgentForward = enableAgentForward
        self.transportPreference = transportPreference
        self.directAddress = directAddress
        self.enablePhysicalMultipath = enablePhysicalMultipath
        self.cellularRemoteAddress = cellularRemoteAddress
        self.enableUpstreamFailover = enableUpstreamFailover
        self.postConnectCommands = postConnectCommands
        self.forwards = forwards
        self.jumpHost = jumpHost
        self.jumpPort = jumpPort
        self.jumpUsername = jumpUsername
        self.jumpKeyEntryId = jumpKeyEntryId
        self.stunServer = stunServer
        self.relayAddr = relayAddr
        self.relaySni = relaySni
        self.relayJwt = relayJwt
        self.allowNonLoopbackForwardBind = allowNonLoopbackForwardBind
        self.themeName = themeName
        self.helperBindPort = helperBindPort
    }

    public mutating func didInsert(_ inserted: InsertionSuccess) {
        id = inserted.rowID
    }
}

/// Phase 1G-1(#53): 定型コマンド(スニペット)。Android版`data.Snippet`相当。
/// `profileId`がnilなら全プロファイル共通、非nilならそのプロファイル専用として表示される
/// (Android版と同じくFK制約は付けない — プロファイル削除時に孤立したスニペットが
/// 残っても単に「該当プロファイルが見つからない」だけで実害が無いため)。
public struct Snippet: Codable, Equatable, Hashable, FetchableRecord, MutablePersistableRecord {
    public var id: Int64?
    public var label: String
    public var command: String
    public var sortOrder: Int
    public var profileId: Int64?
    public var appendNewline: Bool

    public static let databaseTableName = "snippets"

    public init(
        id: Int64? = nil,
        label: String,
        command: String,
        sortOrder: Int = 0,
        profileId: Int64? = nil,
        appendNewline: Bool = true
    ) {
        self.id = id
        self.label = label
        self.command = command
        self.sortOrder = sortOrder
        self.profileId = profileId
        self.appendNewline = appendNewline
    }

    public mutating func didInsert(_ inserted: InsertionSuccess) {
        id = inserted.rowID
    }
}

/// `SnippetCommands.toBytes(command:appendNewline:)`本体は`TsshCoreLogic`
/// (`Sources/TsshCoreLogic/SnippetCommands.swift`)に切り出し済み(GRDBに依存しない
/// 純粋関数なのでLinuxでも`swift test`可能)。ここではGRDBの`Snippet`レコード型に
/// 依存するオーバーロードだけを追加する。
extension SnippetCommands {
    public static func toBytes(snippet: Snippet) -> Data {
        toBytes(command: snippet.command, appendNewline: snippet.appendNewline)
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
        migrator.registerMigration("v2_add_transport_and_jump_fields") { db in
            try db.alter(table: "connection_profile") { t in
                t.add(column: "enableAgentForward", .boolean).notNull().defaults(to: false)
                t.add(column: "transportPreference", .text).notNull().defaults(to: StoredTransportPreference.plainSsh.rawValue)
                t.add(column: "directAddress", .text)
                t.add(column: "enablePhysicalMultipath", .boolean).notNull().defaults(to: false)
                t.add(column: "cellularRemoteAddress", .text)
                t.add(column: "enableUpstreamFailover", .boolean).notNull().defaults(to: false)
                t.add(column: "postConnectCommands", .text)
                t.add(column: "forwards", .text).notNull().defaults(to: "[]")
                t.add(column: "jumpHost", .text)
                t.add(column: "jumpPort", .integer).notNull().defaults(to: 22)
                t.add(column: "jumpUsername", .text)
                t.add(column: "jumpKeyEntryId", .text)
                t.add(column: "stunServer", .text)
                t.add(column: "relayAddr", .text)
                t.add(column: "relaySni", .text)
                t.add(column: "relayJwt", .text)
                t.add(column: "allowNonLoopbackForwardBind", .boolean).notNull().defaults(to: false)
                t.add(column: "themeName", .text)
                t.add(column: "helperBindPort", .integer)
            }
        }
        migrator.registerMigration("v3_create_snippets") { db in
            try db.create(table: "snippets") { t in
                t.autoIncrementedPrimaryKey("id")
                t.column("label", .text).notNull()
                t.column("command", .text).notNull()
                t.column("sortOrder", .integer).notNull().defaults(to: 0)
                t.column("profileId", .integer)
                t.column("appendNewline", .boolean).notNull().defaults(to: true)
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

    // MARK: - Snippet CRUD

    public func insert(snippet: inout Snippet) throws {
        try dbQueue.write { db in try snippet.insert(db) }
    }

    public func update(snippet: Snippet) throws {
        try dbQueue.write { db in try snippet.update(db) }
    }

    public func deleteSnippet(id: Int64) throws {
        _ = try dbQueue.write { db in try Snippet.deleteOne(db, key: id) }
    }

    /// Android版`SnippetDao.getAll`と同じ並び順(sortOrder ASC, label ASC)。
    public func fetchAllSnippets() throws -> [Snippet] {
        try dbQueue.read { db in
            try Snippet.order(Column("sortOrder"), Column("label")).fetchAll(db)
        }
    }

    /// Android版`SnippetDao.getForProfile`相当: 全プロファイル共通(`profileId == nil`)
    /// のスニペットと、指定した`profileId`専用のスニペットの両方を返す。
    public func fetchSnippets(forProfileId profileId: Int64?) throws -> [Snippet] {
        try dbQueue.read { db in
            try Snippet
                .filter(Column("profileId") == nil || Column("profileId") == profileId)
                .order(Column("sortOrder"), Column("label"))
                .fetchAll(db)
        }
    }

    public func fetchSnippet(id: Int64) throws -> Snippet? {
        try dbQueue.read { db in try Snippet.fetchOne(db, key: id) }
    }
}
