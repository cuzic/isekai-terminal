import CryptoKit
import Foundation
import TsshCoreLogic

/// Phase 1E-6(#45): MASQUE relay経由P2P QUIC接続用のJWT(`ConnectionProfile.relayJwt`)を
/// GRDBに保存する前に暗号化するための薄いラッパー。Android版`RelayCredentialVault`+
/// `KeystoreKek`相当で、`CredentialVault`と同じKeychain由来のAES-GCM鍵機構
/// (`KeychainKEKStore`)を、秘密鍵ごとではなく固定の1鍵で再利用する。
///
/// 本格的なcredential vault(`access_jwt`の短命化・`refresh_token`/`device_token`の
/// 発行・revoke/rotate、relay認可サーバー前提の設計、PLAN.md Phase 12以降の設計候補)が
/// 実装されるまでの、平文保存を無くすことだけを目的とした最小限の対策
/// (Android版のissue #1対応コメント参照)。
public final class RelayCredentialVault {
    public enum VaultError: Error, Equatable {
        case keychainError(OSStatus)
        case decryptionFailed
    }

    private let store: KeychainKEKStore
    private let keyId = "relay-jwt-kek"

    public init(keychainService: String = "tools.isekai.terminal.relaycredentialvault") {
        self.store = KeychainKEKStore(service: keychainService)
    }

    public func encrypt(_ plainJwt: String) throws -> String {
        let kek = try store.getOrCreateKey(keyId: keyId)
        let sealedBox = try AES.GCM.seal(Data(plainJwt.utf8), using: kek)
        guard let combined = sealedBox.combined else { throw VaultError.decryptionFailed }
        return combined.base64EncodedString()
    }

    public func decrypt(_ storedValue: String) throws -> String {
        guard let combined = Data(base64Encoded: storedValue) else { throw VaultError.decryptionFailed }
        let kek = try store.getExistingKey(keyId: keyId)
        let sealedBox = try AES.GCM.SealedBox(combined: combined)
        let plaintext = try AES.GCM.open(sealedBox, using: kek)
        guard let jwt = String(data: plaintext, encoding: .utf8) else { throw VaultError.decryptionFailed }
        return jwt
    }
}
