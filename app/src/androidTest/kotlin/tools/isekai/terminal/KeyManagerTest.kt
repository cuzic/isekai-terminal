package tools.isekai.terminal

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import tools.isekai.terminal.KeystoreKek
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.security.KeyStore
import javax.crypto.KeyGenerator

@RunWith(AndroidJUnit4::class)
class KeyManagerTest {

    private val pemBytes =
        "-----BEGIN OPENSSH PRIVATE KEY-----\nfakekey\n-----END OPENSSH PRIVATE KEY-----".toByteArray()

    private val context get() = InstrumentationRegistry.getInstrumentation().targetContext
    private val keyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }

    @Before
    fun setup() {
        keyStore.deleteEntry("tssh_kek_v1")
        val keyGen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        keyGen.init(
            KeyGenParameterSpec.Builder(
                "tssh_kek_v1",
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
        keyStore.deleteEntry("tssh_kek_v1")
    }

    @Test
    fun saveEncryptedKey_createsFile() {
        val path = KeyManager.saveEncryptedKey(context, pemBytes)
        assertTrue(File(path).isFile)
    }

    @Test
    fun saveEncryptedKey_fileIsNotPlaintext() {
        val path = KeyManager.saveEncryptedKey(context, pemBytes)
        assertFalse(File(path).readBytes().contentEquals(pemBytes))
    }

    @Test
    fun saveEncryptedKey_fileDecryptsToOriginal() {
        val path = KeyManager.saveEncryptedKey(context, pemBytes)
        val decrypted = KeystoreKek.decrypt(File(path).readBytes())
        assertArrayEquals(pemBytes, decrypted)
    }

    @Test
    fun saveEncryptedKey_filesInKeysDir() {
        val path = KeyManager.saveEncryptedKey(context, pemBytes)
        val keysDir = File(context.filesDir, "keys").absolutePath
        assertTrue(path.startsWith(keysDir))
    }

    @Test
    fun extractPublicKeyHint_returnsNonEmpty() {
        assertTrue(KeyManager.extractPublicKeyHint(pemBytes).isNotBlank())
    }

    @Test
    fun saveEncryptedKey_multipleCalls_createsSeparateFiles() {
        val first = KeyManager.saveEncryptedKey(context, pemBytes)
        val second = KeyManager.saveEncryptedKey(context, pemBytes)
        assertNotEquals(first, second)
        assertTrue(File(first).isFile)
        assertTrue(File(second).isFile)
    }
}
