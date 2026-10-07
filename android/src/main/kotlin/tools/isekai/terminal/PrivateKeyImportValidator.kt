package tools.isekai.terminal

import java.io.InputStream
import java.nio.ByteBuffer

/**
 * AND-L4: インポートされた秘密鍵ファイルの最小限の検証。以前は任意ファイルをサイズ上限なしで
 * 全量読み込み、PEMとして妥当か・パスフレーズ付きか(このアプリは未対応、`SshAuth.PublicKey`は
 * パスフレーズを持たない)を確認しないまま暗号化保存していたため、誤ったファイルやパスフレーズ付き鍵は
 * 実際の接続時まで失敗に気づけなかった。
 */
object PrivateKeyImportValidator {
    /** 秘密鍵ファイルとして受け付ける最大サイズ。RSA 16384bitのPEMでも十数KBに収まる。 */
    const val MAX_KEY_FILE_BYTES = 64 * 1024

    sealed class Result {
        object Ok : Result()
        data class Error(val message: String) : Result()
    }

    /** [input]から最大[MAX_KEY_FILE_BYTES]+1バイトだけ読む(超過判定用に1バイト余分に読む)。 */
    fun readBounded(input: InputStream): ByteArray {
        val buf = ByteArray(MAX_KEY_FILE_BYTES + 1)
        var total = 0
        while (total < buf.size) {
            val n = input.read(buf, total, buf.size - total)
            if (n == -1) break
            total += n
        }
        return buf.copyOf(total).also { buf.fill(0) }
    }

    fun validate(bytes: ByteArray): Result {
        if (bytes.isEmpty()) return Result.Error("ファイルが空です")
        if (bytes.size > MAX_KEY_FILE_BYTES) return Result.Error("ファイルが大きすぎます(秘密鍵ファイルではありません)")
        val text = String(bytes, Charsets.US_ASCII)
        val begin = Regex("-----BEGIN ([A-Z0-9 ]*PRIVATE KEY)-----").find(text)
            ?: return Result.Error("PEM形式の秘密鍵ではありません(-----BEGIN ... PRIVATE KEY----- が見つかりません)")
        val label = begin.groupValues[1]
        if (!text.contains("-----END $label-----")) return Result.Error("PEMの終端(-----END $label-----)が見つかりません")
        if (label == "ENCRYPTED PRIVATE KEY" || text.contains("Proc-Type: 4,ENCRYPTED")) {
            return Result.Error("パスフレーズ付きの鍵には未対応です(パスフレーズを外してからインポートしてください)")
        }
        if (label == "OPENSSH PRIVATE KEY") {
            val body = text.substring(begin.range.last + 1, text.indexOf("-----END $label-----"))
            val decoded = runCatching { java.util.Base64.getMimeDecoder().decode(body.trim()) }.getOrNull()
                ?: return Result.Error("OpenSSH秘密鍵の本体を読み取れません")
            try {
                val cipher = openSshCipherName(decoded) ?: return Result.Error("OpenSSH秘密鍵の形式が不正です")
                if (cipher != "none") {
                    return Result.Error("パスフレーズ付きの鍵には未対応です(パスフレーズを外してからインポートしてください)")
                }
            } finally {
                decoded.fill(0)
            }
        }
        return Result.Ok
    }

    /** openssh-key-v1の先頭(マジック + cipher名)からcipher名を取り出す。 */
    private fun openSshCipherName(decoded: ByteArray): String? {
        val magic = "openssh-key-v1\u0000".toByteArray(Charsets.US_ASCII)
        if (decoded.size < magic.size + 4) return null
        if (!decoded.copyOfRange(0, magic.size).contentEquals(magic)) return null
        val len = ByteBuffer.wrap(decoded, magic.size, 4).int
        if (len < 0 || len > 64 || magic.size + 4 + len > decoded.size) return null
        return String(decoded, magic.size + 4, len, Charsets.US_ASCII)
    }
}
