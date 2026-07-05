package tools.isekai.terminal

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.security.keystore.StrongBoxUnavailableException
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

object KeystoreKek {

    private const val KEYSTORE_PROVIDER = "AndroidKeyStore"
    // v1 は setUserAuthenticationRequired(true) 付きで生成していたため、
    // 個人利用アプリとしては使用時認証の要件が厳しすぎた（30秒の有効期限切れで
    // 常に UserNotAuthenticatedException になる UX 上の欠陥があった）。
    // v2 では使用時認証を要求せず、Keystore による保存時保護のみに変更。
    private const val KEY_ALIAS = "isekai_terminal_kek_v2"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val GCM_TAG_LENGTH = 128

    fun generateKekIfAbsent() {
        val keyStore = KeyStore.getInstance(KEYSTORE_PROVIDER).apply { load(null) }
        if (keyStore.containsAlias(KEY_ALIAS)) return

        val keyGen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE_PROVIDER)
        val spec = KeyGenParameterSpec.Builder(
            KEY_ALIAS,
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
            .setKeySize(256)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setIsStrongBoxBacked(true)
            .build()
        try {
            keyGen.init(spec)
            keyGen.generateKey()
        } catch (e: StrongBoxUnavailableException) {
            val fallbackSpec = KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setKeySize(256)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .build()
            keyGen.init(fallbackSpec)
            keyGen.generateKey()
        }
    }

    fun encrypt(plaintext: ByteArray): ByteArray {
        val key = loadKey()
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, key)
        val iv = cipher.iv
        val ciphertext = cipher.doFinal(plaintext)
        return iv + ciphertext
    }

    fun decrypt(ivAndCiphertext: ByteArray): ByteArray {
        val key = loadKey()
        val iv = ivAndCiphertext.copyOfRange(0, 12)
        val ciphertext = ivAndCiphertext.copyOfRange(12, ivAndCiphertext.size)
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(GCM_TAG_LENGTH, iv))
        return cipher.doFinal(ciphertext)
    }

    private fun loadKey(): SecretKey {
        val keyStore = KeyStore.getInstance(KEYSTORE_PROVIDER).apply { load(null) }
        return (keyStore.getEntry(KEY_ALIAS, null) as KeyStore.SecretKeyEntry).secretKey
    }
}
