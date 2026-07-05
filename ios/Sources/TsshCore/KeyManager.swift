import Foundation
import CryptoKit

/// Phase 1D: ed25519鍵ペア生成 + OpenSSH private key PEM形式のエンコード。
/// Android版`KeyManager.kt`(`generateEd25519Pair`/`buildOpenSshPrivateKeyPem`/
/// `buildAuthorizedKeysLine`)の1:1移植。CryptoKitの`Curve25519.Signing.PrivateKey`は
/// `rawRepresentation`(32byte seed)と`publicKey.rawRepresentation`(32byte)を返すため、
/// Android版がPKCS8/SPKI DERから切り出していた raw scalar/pubkey とそのまま対応する。
///
/// インポートされた既存の秘密鍵からの公開鍵抽出はAndroid版でも実装されておらず
/// (`extractPublicKeyHint`はプレースホルダー文言を返すだけ)、この移植でも同様に
/// 実際のPEM解析は行わない(サーバー側のauthorized_keysを直接確認してもらう運用)。
public enum KeyManager {

    /// ed25519鍵ペアを生成し、(OpenSSH private key PEM bytes, authorized_keys line)を返す。
    public static func generateEd25519Pair() -> (pemBytes: Data, authorizedKeysLine: String) {
        let privateKey = Curve25519.Signing.PrivateKey()
        let privRaw = privateKey.rawRepresentation
        let pubRaw = privateKey.publicKey.rawRepresentation

        let pemBytes = buildOpenSshPrivateKeyPem(privRaw: privRaw, pubRaw: pubRaw)
        let authorizedKeysLine = buildAuthorizedKeysLine(pubRaw: pubRaw)
        return (pemBytes, authorizedKeysLine)
    }

    /// インポートされた鍵から公開鍵を抽出できないため、案内文言を返す(Android版と同じ運用)。
    public static func extractPublicKeyHint(pemBytes: Data) -> String {
        "(公開鍵はサーバー側の authorized_keys を直接確認してください)"
    }

    /// `ssh-ed25519 <base64>`形式のauthorized_keys行を返す。
    public static func buildAuthorizedKeysLine(pubRaw: Data) -> String {
        let keyType = Data("ssh-ed25519".utf8)
        let pubWire = sshStr(keyType) + sshStr(pubRaw)
        return "ssh-ed25519 \(pubWire.base64EncodedString())"
    }

    // MARK: - OpenSSH wire format helpers

    private static func sshUint32(_ n: UInt32) -> Data {
        let be = n.bigEndian
        return withUnsafeBytes(of: be) { Data($0) }
    }

    private static func sshStr(_ bytes: Data) -> Data {
        sshUint32(UInt32(bytes.count)) + bytes
    }

    /// rawなed25519鍵ペアをOpenSSH private key PEMとしてエンコードする。
    /// russhの`PrivateKey::from_openssh()`が読める形式(Android版と同じ)。
    private static func buildOpenSshPrivateKeyPem(privRaw: Data, pubRaw: Data) -> Data {
        // AUTH_MAGIC (OpenSSH PROTOCOL.key): "openssh-key-v1" + 終端NUL、合わせて15バイト。
        // (Android版`KeyManager.kt`は末尾がNULではなく半角スペースになっているが、
        // これは仕様上誤り。ここでは仕様通りに修正して移植する。)
        let magic = Data("openssh-key-v1".utf8) + Data([0x00])
        let keyType = Data("ssh-ed25519".utf8)
        let pubWire = sshStr(keyType) + sshStr(pubRaw)

        let checkInt = UInt32(truncatingIfNeeded: Int64(Date().timeIntervalSince1970 * 1000))

        var privSection = Data()
        privSection += sshUint32(checkInt)
        privSection += sshUint32(checkInt)
        privSection += sshStr(keyType)
        privSection += sshStr(pubRaw)
        privSection += sshStr(privRaw + pubRaw) // 64 bytes: seed || pubkey
        privSection += sshStr(Data()) // empty comment
        // Padding: 1,2,3,... until block-aligned (block size = 8 for cipher=none)
        var pad: UInt8 = 1
        while privSection.count % 8 != 0 {
            privSection.append(pad)
            pad += 1
        }

        var body = Data()
        body += magic
        body += sshStr(Data("none".utf8)) // cipher
        body += sshStr(Data("none".utf8)) // kdf
        body += sshStr(Data()) // kdf options
        body += sshUint32(1) // num keys
        body += sshStr(pubWire)
        body += sshStr(privSection)

        let b64 = body.base64EncodedString()
        let wrapped = stride(from: 0, to: b64.count, by: 70).map { start -> String in
            let startIdx = b64.index(b64.startIndex, offsetBy: start)
            let endIdx = b64.index(startIdx, offsetBy: 70, limitedBy: b64.endIndex) ?? b64.endIndex
            return String(b64[startIdx..<endIdx])
        }.joined(separator: "\n")

        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\n\(wrapped)\n-----END OPENSSH PRIVATE KEY-----\n"
        return Data(pem.utf8)
    }
}
