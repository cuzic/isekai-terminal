import CryptoKit
import Foundation
import Security
import TsshCoreLogic

/// Phase 1B: SSH秘密鍵・パスワード・passphrase・helper認証情報等を保護するための
/// Vault(ChatGPT外部レビュー2026-07-04で#9aから拡張、PLAN.md「Phase Y」節参照)。
///
/// 構成: 秘密材料をAES-GCMで暗号化しアプリ管理下のファイルへ保存し、その暗号鍵(KEK)を
/// Keychain(`kSecAttrAccessibleWhenUnlockedThisDeviceOnly`)へ保存する。Secure Enclave
/// (生体認証必須モード・Secure Enclave生成P-256鍵)はオプション扱いで、ここでは実装しない
/// (NIST P-256のみ対応でインポート済みEd25519/RSA/ECDSA鍵を格納できないため)。
public final class CredentialVault {
    public enum VaultError: Error, Equatable {
        case keychainError(OSStatus)
        case blobNotFound
        case decryptionFailed
        case deviceLocked
        case unsupportedFormatVersion(UInt8)
    }

    /// Vaultで保護する1件の秘密材料に紐づくメタデータ。DBには秘密材料そのものを
    /// 保存せずこのメタデータのみを保存する想定(#10のプロファイル管理画面参照)。
    public struct Metadata {
        public let keyId: String
        public let keyType: String
        public let publicKey: String

        public init(keyId: String, keyType: String, publicKey: String) {
            self.keyId = keyId
            self.keyType = keyType
            self.publicKey = publicKey
        }
    }

    private static let formatVersion: UInt8 = 1
    private let blobDirectory: URL
    private let keychainService: String

    public init(blobDirectory: URL, keychainService: String = "tools.isekai.terminal.credentialvault") throws {
        self.blobDirectory = blobDirectory
        self.keychainService = keychainService
        try FileManager.default.createDirectory(at: blobDirectory, withIntermediateDirectories: true)
    }

    /// 秘密材料を暗号化して保存する。KEKが未生成なら新規生成しKeychainへ保存する。
    /// blobの保存に失敗した場合、今回新規生成したKEKはロールバック(削除)する
    /// (孤立したKeychainエントリを残さないため)。
    public func store(secret: Data, metadata: Metadata) throws {
        let keyExistedBefore = (try? keychain.getExistingKey(keyId: metadata.keyId)) != nil
        let kek = try keychain.getOrCreateKey(keyId: metadata.keyId)
        do {
            let envelope = try seal(secret: secret, using: kek, metadata: metadata)
            try atomicWrite(envelope, to: blobPath(for: metadata.keyId))
        } catch {
            if !keyExistedBefore {
                try? keychain.deleteKey(keyId: metadata.keyId)
            }
            throw error
        }
    }

    /// 保存済みの秘密材料を復号して取り出す。
    public func retrieve(metadata: Metadata) throws -> Data {
        let path = blobPath(for: metadata.keyId)
        guard FileManager.default.fileExists(atPath: path.path) else {
            throw VaultError.blobNotFound
        }
        let envelope = try Data(contentsOf: path)
        let kek = try keychain.getExistingKey(keyId: metadata.keyId)
        return try open(envelope: envelope, using: kek, metadata: metadata)
    }

    /// 秘密材料とKEKの両方を削除する。
    public func delete(keyId: String) throws {
        try? FileManager.default.removeItem(at: blobPath(for: keyId))
        try keychain.deleteKey(keyId: keyId)
    }

    /// KEKを再生成し、既存のblobを新しいKEKで暗号化し直す(鍵ローテーション)。
    public func rotateKey(metadata: Metadata) throws {
        let plaintext = try retrieve(metadata: metadata)
        try keychain.deleteKey(keyId: metadata.keyId)
        try store(secret: plaintext, metadata: metadata)
    }

    /// アプリ起動時に呼び、`knownKeyIds`に含まれないblobファイル(孤立ファイル)を削除する。
    public func cleanupOrphanBlobs(knownKeyIds: Set<String>) throws {
        let knownHashes = Set(knownKeyIds.map { Self.hashedFilename(for: $0) })
        let files = try FileManager.default.contentsOfDirectory(at: blobDirectory, includingPropertiesForKeys: nil)
        for file in files {
            let name = file.deletingPathExtension().lastPathComponent
            if !knownHashes.contains(name) {
                try? FileManager.default.removeItem(at: file)
            }
        }
    }

    // MARK: - 暗号化/復号

    private func seal(secret: Data, using kek: SymmetricKey, metadata: Metadata) throws -> Data {
        let aad = aadData(for: metadata)
        let sealedBox = try AES.GCM.seal(secret, using: kek, authenticating: aad)
        guard let combined = sealedBox.combined else { throw VaultError.decryptionFailed }
        var envelope = Data([Self.formatVersion])
        envelope.append(combined)
        return envelope
    }

    private func open(envelope: Data, using kek: SymmetricKey, metadata: Metadata) throws -> Data {
        guard let version = envelope.first else { throw VaultError.decryptionFailed }
        guard version == Self.formatVersion else { throw VaultError.unsupportedFormatVersion(version) }
        let combined = envelope.dropFirst()
        let aad = aadData(for: metadata)
        let sealedBox = try AES.GCM.SealedBox(combined: combined)
        return try AES.GCM.open(sealedBox, using: kek, authenticating: aad)
    }

    /// key_id/key_type/public_keyをAAD(認証付き加算データ)へ含める。これにより、
    /// 暗号文自体は正しくても異なるkey_id/key_typeの下で復号しようとすると失敗する
    /// (blobの取り違え・すり替えを検知できる)。
    private func aadData(for metadata: Metadata) -> Data {
        var aad = Data()
        for field in [metadata.keyId, metadata.keyType, metadata.publicKey] {
            aad.append(Data(field.utf8))
            aad.append(0)
        }
        return aad
    }

    // MARK: - ファイルパス

    /// 絶対パスをkey_idから直接組み立てず、ハッシュ化したファイル名を使う
    /// (呼び出し側が渡すkey_idにpath traversal的な文字列が混入しても安全)。
    private func blobPath(for keyId: String) -> URL {
        blobDirectory
            .appendingPathComponent(Self.hashedFilename(for: keyId))
            .appendingPathExtension("cvault")
    }

    private static func hashedFilename(for keyId: String) -> String {
        let digest = SHA256.hash(data: Data(keyId.utf8))
        return digest.map { String(format: "%02x", $0) }.joined()
    }

    private func atomicWrite(_ data: Data, to url: URL) throws {
        try data.write(to: url, options: .atomic)
    }

    private var keychain: KeychainKEKStore { KeychainKEKStore(service: keychainService) }
}

/// KEK(Key Encryption Key)のKeychain保管を担当する。`CredentialVault`以外からは
/// 直接使わない想定。
struct KeychainKEKStore {
    let service: String

    func getOrCreateKey(keyId: String) throws -> SymmetricKey {
        if let existing = try? getExistingKey(keyId: keyId) {
            return existing
        }
        let key = SymmetricKey(size: .bits256)
        try storeKey(key, keyId: keyId)
        return key
    }

    func getExistingKey(keyId: String) throws -> SymmetricKey {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: keyId,
            kSecReturnData as String: true,
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecInteractionNotAllowed {
            throw CredentialVault.VaultError.deviceLocked
        }
        guard status == errSecSuccess, let data = result as? Data else {
            throw CredentialVault.VaultError.keychainError(status)
        }
        return SymmetricKey(data: data)
    }

    func storeKey(_ key: SymmetricKey, keyId: String) throws {
        let keyData = key.withUnsafeBytes { Data($0) }
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: keyId,
            kSecValueData as String: keyData,
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
        ]
        // 既存があれば消してから追加する(SecItemAddは重複でerrSecDuplicateItemになるため)。
        SecItemDelete(query as CFDictionary)
        let status = SecItemAdd(query as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw CredentialVault.VaultError.keychainError(status)
        }
    }

    func deleteKey(keyId: String) throws {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: keyId,
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw CredentialVault.VaultError.keychainError(status)
        }
    }
}
