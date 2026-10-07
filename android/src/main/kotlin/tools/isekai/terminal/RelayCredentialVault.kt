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

    /**
     * [storedValue]を平文JWTへ戻す。
     *
     * AND-H3: 暗号化導入(`400df8c1`)以前に平文のまま保存されたJWTが残っている場合、
     * 以前はBase64デコードの`IllegalArgumentException`が接続コルーチンから未捕捉のまま
     * 抜けてアプリごとクラッシュしていた(平文→暗号化のデータ移行は存在しない)。
     * 暗号文は`Base64.NO_WRAP`で保存しており、その字種に`.`は含まれない一方、JWTは
     * 必ず`.`区切りの3セグメントになるため、`.`を含む値は平文レガシー値としてそのまま
     * 返す(次に編集画面で保存した時点で暗号化される)。それ以外の復号失敗
     * (Keystoreエントリ欠落・改竄等)は原因の分かる[IllegalStateException]にする。
     */
    fun decrypt(storedValue: String): String = decryptWith(storedValue) { KeystoreKek.decrypt(it) }

    /** [decrypt]の本体。AndroidKeyStoreを使わずにテストできるよう復号関数を注入可能にしている。 */
    internal fun decryptWith(storedValue: String, decryptBytes: (ByteArray) -> ByteArray): String {
        if (isLegacyPlaintext(storedValue)) return storedValue
        return try {
            val enc = Base64.decode(storedValue, Base64.NO_WRAP)
            String(decryptBytes(enc), Charsets.UTF_8)
        } catch (e: Exception) {
            throw IllegalStateException("relay JWTを復号できませんでした(${e.javaClass.simpleName})。プロファイルを編集してJWTを再入力してください", e)
        }
    }

    /** 編集画面の初期表示用。復号できない値は空欄に落とす(クラッシュさせない)。 */
    fun decryptOrEmpty(storedValue: String): String = runCatching { decrypt(storedValue) }.getOrDefault("")

    internal fun isLegacyPlaintext(storedValue: String): Boolean = storedValue.contains('.')
}
