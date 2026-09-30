import Foundation

/// Phase 1B: SSH/helper信頼ストア。秘密鍵管理(CredentialVault)はあったが
/// 接続先サーバーを信頼する仕組みが抜けていた点を埋める
/// (ChatGPT外部レビュー2026-07-04、PLAN.md「Phase Y」節参照)。
///
/// 管理対象を1つの名前空間に混在させず、種別ごとに識別子を分ける:
/// SSH host key・isekai-helper identity・踏み台(ProxyJump)ホスト鍵。
/// `direct_address`のような「別名」も、正規化した識別子にまとめることで
/// 同一ホストとして扱える。
public enum TrustIdentifierKind: String, Codable {
    case sshHost
    case isekaiHelperIdentity
    case jumpHost
}

/// 信頼済みホストの1レコード。
public struct TrustedHostRecord: Codable, Equatable {
    public let identifier: String
    public let keyType: String
    public let fingerprint: String
    public let firstTrustedAt: Date

    public init(identifier: String, keyType: String, fingerprint: String, firstTrustedAt: Date) {
        self.identifier = identifier
        self.keyType = keyType
        self.fingerprint = fingerprint
        self.firstTrustedAt = firstTrustedAt
    }
}

/// ホスト鍵確認の結果。呼び出し側(UI)はこれを見て挙動を変える:
/// - `.trustedMatch`: 自動許可してよい
/// - `.unknownHost`: 初回接続。fingerprint等を表示してユーザー承認を求める
/// - `.mismatch`: 自動許可しない。明示的な警告(旧鍵と新鍵の両方を表示)が必要
public enum HostKeyVerificationResult: Equatable {
    case trustedMatch
    case unknownHost
    case mismatch(previousFingerprint: String, previousKeyType: String)
}

/// SSH/helper信頼ストア本体。GRDB統合(#10)より前に着手するため、まずは
/// JSONファイルへの永続化(atomic write)で実装する。将来GRDB化する場合も
/// このpublic APIは変えずに内部実装だけ差し替えられる設計にしている。
///
/// スレッド安全性: `verify`/`trust`はRustのtokioスレッド上の`onHostKey`コールバックから、
/// `trustNewHostKey`等はmainから呼ばれ、しかもストアは全タブで共有される。`records`への
/// 全アクセスを`lock`で直列化する(2026-09-29レビューIOS-I2。以前は同期が無く、
/// 複数タブの同時接続でDictionaryへのデータ競合が起こりえた)。
public final class SshHostTrustStore: @unchecked Sendable {
    /// 起動時の読み込みで既存ファイルが壊れていて、空のストアで開き直した場合の情報。
    public struct RecoveredCorruption {
        /// 壊れていたファイルの退避先(退避自体に失敗した場合はnil)。
        public let quarantinedURL: URL?
        public let error: Error
    }

    private let storeURL: URL
    private let lock = NSLock()
    private var records: [String: TrustedHostRecord]

    public init(storeURL: URL) throws {
        self.storeURL = storeURL
        self.records = try Self.load(from: storeURL)
    }

    private init(storeURL: URL, records: [String: TrustedHostRecord]) {
        self.storeURL = storeURL
        self.records = records
    }

    /// 既存ファイルが読めない(JSONが壊れている等)場合に、起動不能にせず空のストアで開く。
    /// 壊れたファイルは`<name>.corrupt-<unix秒>`へ退避して残す(後から調べられるように)。
    /// 信頼レコードを失うと既知ホストも初回接続扱いに戻る(再度TOFU確認が出る)が、
    /// 以前のように起動のたびにfatalErrorで落ち、再インストール(=全プロファイル・鍵の
    /// 消失)以外に復旧手段が無い状態よりは安全(2026-09-29レビューIOS-I3)。
    public static func openRecoveringCorruption(storeURL: URL) -> (store: SshHostTrustStore, recovered: RecoveredCorruption?) {
        do {
            return (try SshHostTrustStore(storeURL: storeURL), nil)
        } catch {
            let quarantine = storeURL.deletingLastPathComponent().appendingPathComponent(
                "\(storeURL.lastPathComponent).corrupt-\(Int(Date().timeIntervalSince1970))"
            )
            let moved: URL? = (try? FileManager.default.moveItem(at: storeURL, to: quarantine)) != nil ? quarantine : nil
            return (
                SshHostTrustStore(storeURL: storeURL, records: [:]),
                RecoveredCorruption(quarantinedURL: moved, error: error)
            )
        }
    }

    /// `kind`/`host`/`port`から一意な識別子を作る。ホスト名は大文字小文字を
    /// 区別しないSSHの慣例に合わせ小文字化して正規化する。
    public static func makeIdentifier(kind: TrustIdentifierKind, host: String, port: UInt16) -> String {
        "\(kind.rawValue)|\(host.lowercased()):\(port)"
    }

    /// ホスト鍵を検証する(接続時にRust側`onHostKey`callbackから呼ばれる想定)。
    public func verify(identifier: String, keyType: String, fingerprint: String) -> HostKeyVerificationResult {
        guard let existing = withLock({ records[identifier] }) else {
            return .unknownHost
        }
        if existing.fingerprint == fingerprint && existing.keyType == keyType {
            return .trustedMatch
        }
        return .mismatch(previousFingerprint: existing.fingerprint, previousKeyType: existing.keyType)
    }

    /// ユーザーが承認した後に呼ぶ。`.mismatch`だったケースも含め、既存レコードを
    /// 明示的に上書きするのはこの呼び出し経由のみで、自動上書きは行わない。
    /// 永続化に失敗した場合はメモリ上の状態も変更しない(ファイルとメモリの食い違いを作らない)。
    public func trust(identifier: String, keyType: String, fingerprint: String) throws {
        let record = TrustedHostRecord(
            identifier: identifier,
            keyType: keyType,
            fingerprint: fingerprint,
            firstTrustedAt: Date()
        )
        try withLock {
            var updated = records
            updated[identifier] = record
            try Self.save(updated, to: storeURL)
            records = updated
        }
    }

    /// 信頼を取り消す(ホスト鍵の再確認をやり直したい場合等)。
    public func revoke(identifier: String) throws {
        try withLock {
            var updated = records
            updated.removeValue(forKey: identifier)
            try Self.save(updated, to: storeURL)
            records = updated
        }
    }

    public func record(for identifier: String) -> TrustedHostRecord? {
        withLock { records[identifier] }
    }

    public var allRecords: [TrustedHostRecord] {
        withLock { Array(records.values) }
    }

    private func withLock<T>(_ body: () throws -> T) rethrows -> T {
        lock.lock()
        defer { lock.unlock() }
        return try body()
    }

    private static func save(_ records: [String: TrustedHostRecord], to url: URL) throws {
        let data = try JSONEncoder().encode(Array(records.values))
        try data.write(to: url, options: .atomic)
    }

    private static func load(from url: URL) throws -> [String: TrustedHostRecord] {
        guard FileManager.default.fileExists(atPath: url.path) else { return [:] }
        let data = try Data(contentsOf: url)
        let list = try JSONDecoder().decode([TrustedHostRecord].self, from: data)
        // 同じidentifierが重複していても(手編集・過去の不具合等)trapしないよう後勝ちで読む。
        // `Dictionary(uniqueKeysWithValues:)`は重複キーで実行時trapし、起動のたびに
        // クラッシュしていた(2026-09-29レビューIOS-I3)。
        return Dictionary(list.map { ($0.identifier, $0) }, uniquingKeysWith: { _, last in last })
    }
}
