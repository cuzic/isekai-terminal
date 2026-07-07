package tools.isekai.terminal

import android.util.Base64

/**
 * relay_jwt(MASQUE relay経由P2P QUIC接続用のJWT、`ConnectionProfile.relayJwt`)を
 * Roomに保存する前に暗号化するための薄いラッパー。秘密鍵([KeyManager]/[KeystoreKek]参照)と
 * 同じ Android Keystore 由来の KEK(AES/GCM)を再利用する。
 *
 * 本格的な credential vault(`access_jwt` の短命化・メモリ限定保持、`refresh_token`/
 * `device_token` の発行・revoke/rotate、といった relay 認可サーバー前提の設計、
 * PLAN.md Phase 12以降の設計候補)が実装されるまでの、
 * [issue #1](https://github.com/cuzic/isekai-terminal/issues/1) に対する最小限の対策
 * (平文保存を無くすことだけを目的とする)。
 *
 * `AndroidKeyStore` は Robolectric では利用できないため、この object を直接呼ぶのは
 * 実際に Android フレームワークが動く経路(`AndroidAppExecutor`、`ProfileEditScreen` の
 * デフォルト引数)に限る。テストでは呼び出し元(`DumbAppExecutor` や
 * `ProfileEditScreen(encryptRelayJwt = { it }, decryptRelayJwt = { it })`)で
 * 恒等関数に差し替えること。
 */
object RelayCredentialVault {
    fun encrypt(plainJwt: String): String {
        KeystoreKek.generateKekIfAbsent()
        val enc = KeystoreKek.encrypt(plainJwt.toByteArray(Charsets.UTF_8))
        return Base64.encodeToString(enc, Base64.NO_WRAP)
    }

    fun decrypt(storedValue: String): String {
        val enc = Base64.decode(storedValue, Base64.NO_WRAP)
        return String(KeystoreKek.decrypt(enc), Charsets.UTF_8)
    }
}
