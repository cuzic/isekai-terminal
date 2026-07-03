package tools.isekai.terminal

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.security.KeyStore
import javax.crypto.KeyGenerator

@RunWith(AndroidJUnit4::class)
class KeystoreKekTest {

    private val keyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }

    @Before
    fun setup() {
        keyStore.deleteEntry("tssh_kek_v2")
        val keyGen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        keyGen.init(
            KeyGenParameterSpec.Builder(
                "tssh_kek_v2",
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setKeySize(256)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .build()
        )
        keyGen.generateKey()
    }

    @After
    fun cleanup() {
        keyStore.deleteEntry("tssh_kek_v2")
    }

    @Test
    fun encrypt_decrypt_roundtrip_shortData() {
        val plaintext = "hello".toByteArray()
        val decrypted = KeystoreKek.decrypt(KeystoreKek.encrypt(plaintext))
        assertArrayEquals(plaintext, decrypted)
    }

    @Test
    fun encrypt_decrypt_roundtrip_longData() {
        val plaintext = (0..999).map { it.toByte() }.toByteArray()
        val decrypted = KeystoreKek.decrypt(KeystoreKek.encrypt(plaintext))
        assertArrayEquals(plaintext, decrypted)
    }

    @Test
    fun generateKekIfAbsent_idempotent() {
        KeystoreKek.generateKekIfAbsent()
        KeystoreKek.generateKekIfAbsent()
        val plaintext = "still works".toByteArray()
        assertArrayEquals(plaintext, KeystoreKek.decrypt(KeystoreKek.encrypt(plaintext)))
    }

    @Test
    fun encrypt_producesNonPlaintext() {
        val plaintext = "hello".toByteArray()
        val encrypted = KeystoreKek.encrypt(plaintext)
        assertFalse(encrypted.contentEquals(plaintext))
    }

    @Test
    fun encrypt_twiceSameInput_differentCiphertext() {
        val plaintext = "hello".toByteArray()
        val first = KeystoreKek.encrypt(plaintext)
        val second = KeystoreKek.encrypt(plaintext)
        assertFalse(first.contentEquals(second))
    }

    @Test(expected = Exception::class)
    fun decrypt_wrongData_throws() {
        val garbage = ByteArray(28) { it.toByte() } // 12 IV + 16 bytes, wrong tag
        KeystoreKek.decrypt(garbage)
    }
}
