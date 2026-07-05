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
public final class SshHostTrustStore {
    private let storeURL: URL
    private var records: [String: TrustedHostRecord]

    public init(storeURL: URL) throws {
        self.storeURL = storeURL
        self.records = try Self.load(from: storeURL)
    }

    /// `kind`/`host`/`port`から一意な識別子を作る。ホスト名は大文字小文字を
    /// 区別しないSSHの慣例に合わせ小文字化して正規化する。
    public static func makeIdentifier(kind: TrustIdentifierKind, host: String, port: UInt16) -> String {
        "\(kind.rawValue)|\(host.lowercased()):\(port)"
    }

    /// ホスト鍵を検証する(接続時にRust側`onHostKey`callbackから呼ばれる想定)。
    public func verify(identifier: String, keyType: String, fingerprint: String) -> HostKeyVerificationResult {
        guard let existing = records[identifier] else {
            return .unknownHost
        }
        if existing.fingerprint == fingerprint && existing.keyType == keyType {
            return .trustedMatch
        }
        return .mismatch(previousFingerprint: existing.fingerprint, previousKeyType: existing.keyType)
    }

    /// ユーザーが承認した後に呼ぶ。`.mismatch`だったケースも含め、既存レコードを
    /// 明示的に上書きするのはこの呼び出し経由のみで、自動上書きは行わない。
    public func trust(identifier: String, keyType: String, fingerprint: String) throws {
        records[identifier] = TrustedHostRecord(
            identifier: identifier,
            keyType: keyType,
            fingerprint: fingerprint,
            firstTrustedAt: Date()
        )
        try save()
    }

    /// 信頼を取り消す(ホスト鍵の再確認をやり直したい場合等)。
    public func revoke(identifier: String) throws {
        records.removeValue(forKey: identifier)
        try save()
    }

    public func record(for identifier: String) -> TrustedHostRecord? {
        records[identifier]
    }

    public var allRecords: [TrustedHostRecord] {
        Array(records.values)
    }

    private func save() throws {
        let data = try JSONEncoder().encode(Array(records.values))
        try data.write(to: storeURL, options: .atomic)
    }

    private static func load(from url: URL) throws -> [String: TrustedHostRecord] {
        guard FileManager.default.fileExists(atPath: url.path) else { return [:] }
        let data = try Data(contentsOf: url)
        let list = try JSONDecoder().decode([TrustedHostRecord].self, from: data)
        return Dictionary(uniqueKeysWithValues: list.map { ($0.identifier, $0) })
    }
}
