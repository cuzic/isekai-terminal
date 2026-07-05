package tools.isekai.terminal

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.security.KeyStore
import javax.crypto.KeyGenerator

/**
 * [RelayCredentialVault]は[KeystoreKek]の薄いラッパーであり、AndroidKeyStoreを
 * 使うためRobolectric(app/src/test)では動かせない(実際にNoSuchAlgorithmExceptionで
 * 失敗することを確認済み)。[KeystoreKekTest]同様、実機/エミュレータでのみ実行する。
 */
@RunWith(AndroidJUnit4::class)
class RelayCredentialVaultTest {

    private val keyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }

    @Before
    fun setup() {
        keyStore.deleteEntry("isekai_terminal_kek_v2")
        val keyGen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        keyGen.init(
            KeyGenParameterSpec.Builder(
                "isekai_terminal_kek_v2",
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
        keyStore.deleteEntry("isekai_terminal_kek_v2")
    }

    @Test
    fun encrypt_decrypt_roundtrip() {
        val jwt = "eyJhbGciOiJSUzI1NiJ9.test.sig"
        val stored = RelayCredentialVault.encrypt(jwt)
        assertEquals(jwt, RelayCredentialVault.decrypt(stored))
    }

    @Test
    fun encrypt_producesNonPlaintext() {
        val jwt = "eyJhbGciOiJSUzI1NiJ9.test.sig"
        val stored = RelayCredentialVault.encrypt(jwt)
        assertNotEquals(jwt, stored)
    }

    @Test
    fun encrypt_isBase64_decodableWithoutThrowing() {
        val stored = RelayCredentialVault.encrypt("eyJhbGciOiJSUzI1NiJ9.test.sig")
        // NO_WRAP前提: 改行を含まない一行のBase64文字列であること。
        assertEquals(stored.trim(), stored)
        android.util.Base64.decode(stored, android.util.Base64.NO_WRAP)
    }
}
