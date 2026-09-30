package tools.isekai.terminal

import java.io.ByteArrayInputStream
import java.nio.ByteBuffer
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** AND-L4: 鍵インポートの検証。AND-L3: KEKエイリアスのメタデータ整合。 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class PrivateKeyImportValidatorTest {

    private fun openSshPem(cipher: String): ByteArray {
        val magic = "openssh-key-v1\u0000".toByteArray(Charsets.US_ASCII)
        val c = cipher.toByteArray()
        val body = magic + ByteBuffer.allocate(4).putInt(c.size).array() + c + ByteArray(32)
        val b64 = java.util.Base64.getEncoder().encodeToString(body).chunked(70).joinToString("\n")
        return "-----BEGIN OPENSSH PRIVATE KEY-----\n$b64\n-----END OPENSSH PRIVATE KEY-----\n".toByteArray()
    }

    @Test
    fun generatedEd25519Key_isAccepted() {
        val (pem, _) = KeyManager.generateEd25519Pair()
        assertEquals(PrivateKeyImportValidator.Result.Ok, PrivateKeyImportValidator.validate(pem))
    }

    @Test
    fun passphraseProtectedOpenSshKey_isRejected() {
        val r = PrivateKeyImportValidator.validate(openSshPem("aes256-ctr"))
        assertTrue(r is PrivateKeyImportValidator.Result.Error)
        assertTrue((r as PrivateKeyImportValidator.Result.Error).message.contains("パスフレーズ"))
    }

    @Test
    fun unencryptedOpenSshKey_isAccepted() {
        assertEquals(PrivateKeyImportValidator.Result.Ok, PrivateKeyImportValidator.validate(openSshPem("none")))
    }

    @Test
    fun encryptedPkcs8AndLegacyPem_areRejected() {
        val pkcs8 = "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n".toByteArray()
        val legacy = "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nAAAA\n-----END RSA PRIVATE KEY-----\n".toByteArray()
        assertTrue(PrivateKeyImportValidator.validate(pkcs8) is PrivateKeyImportValidator.Result.Error)
        assertTrue(PrivateKeyImportValidator.validate(legacy) is PrivateKeyImportValidator.Result.Error)
    }

    @Test
    fun nonPemFile_isRejected() {
        assertTrue(PrivateKeyImportValidator.validate("hello world".toByteArray()) is PrivateKeyImportValidator.Result.Error)
        assertTrue(PrivateKeyImportValidator.validate(ByteArray(0)) is PrivateKeyImportValidator.Result.Error)
    }

    @Test
    fun oversizedFile_isReadOnlyUpToTheLimitAndRejected() {
        val huge = ByteArray(10 * PrivateKeyImportValidator.MAX_KEY_FILE_BYTES)
        val read = PrivateKeyImportValidator.readBounded(ByteArrayInputStream(huge))
        assertEquals(PrivateKeyImportValidator.MAX_KEY_FILE_BYTES + 1, read.size)
        assertTrue(PrivateKeyImportValidator.validate(read) is PrivateKeyImportValidator.Result.Error)
    }

    @Test
    fun kekAliasMetadata_matchesTheAliasActuallyUsedForEncryption() {
        assertEquals(KeystoreKek.KEY_ALIAS, KeyManager.KEK_ALIAS)
    }
}
