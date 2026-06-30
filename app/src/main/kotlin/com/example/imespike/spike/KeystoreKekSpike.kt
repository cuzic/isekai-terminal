package com.example.imespike.spike

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.security.keystore.StrongBoxUnavailableException
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Android Keystore KEK 方式スパイク。
 *
 * - Keystore で AES-256-GCM 鍵（KEK）を生成・保管
 * - その KEK で秘密鍵バイト列を暗号化
 * - 復号して元のバイト列と一致することを確認
 *
 * 秘密鍵の平文はメモリ上にのみ存在し、ストレージには暗号化済みバイト列のみ保存する。
 */
object KeystoreKek {

    private const val KEYSTORE_PROVIDER = "AndroidKeyStore"
    private const val KEY_ALIAS = "tssh_kek_v1"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val GCM_TAG_LENGTH = 128

    /** KEK を Keystore に生成する（初回のみ）。 */
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
            .setUserAuthenticationRequired(true)
            .setUserAuthenticationValidityDurationSeconds(30)
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
                .setUserAuthenticationRequired(true)
                .setUserAuthenticationValidityDurationSeconds(30)
                .build()
            keyGen.init(fallbackSpec)
            keyGen.generateKey()
        }
    }

    /** KEK で平文を暗号化し、IV + 暗号文を返す。 */
    fun encrypt(plaintext: ByteArray): ByteArray {
        val key = loadKey()
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, key)
        val iv = cipher.iv          // GCM は 12 バイト IV を自動生成
        val ciphertext = cipher.doFinal(plaintext)
        // 先頭 12 バイト = IV、残り = 暗号文 + 16 バイト GCM タグ
        return iv + ciphertext
    }

    /** IV + 暗号文を受け取り、復号して平文を返す。 */
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

    /** スパイク用: 暗号化→復号が元データと一致するか検証する。 */
    fun runSelfTest(privateKeyBytes: ByteArray): Boolean {
        generateKekIfAbsent()
        val encrypted = encrypt(privateKeyBytes)
        val decrypted = decrypt(encrypted)
        return privateKeyBytes.contentEquals(decrypted)
    }
}
