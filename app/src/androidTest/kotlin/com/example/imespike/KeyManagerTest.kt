package com.example.imespike

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import com.example.imespike.spike.KeystoreKek
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File

@RunWith(AndroidJUnit4::class)
class KeyManagerTest {

    private val pemBytes =
        "-----BEGIN OPENSSH PRIVATE KEY-----\nfakekey\n-----END OPENSSH PRIVATE KEY-----".toByteArray()

    private val context get() = InstrumentationRegistry.getInstrumentation().targetContext

    @Before
    fun ensureKey() {
        KeystoreKek.generateKekIfAbsent()
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
