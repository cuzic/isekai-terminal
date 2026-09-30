package tools.isekai.terminal

import android.util.Base64
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * AND-H3: [RelayCredentialVault.decryptWith]の復号失敗・平文レガシー値の扱い。
 * AndroidKeyStoreはRobolectricで使えないため、復号関数を注入して検証する。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class RelayCredentialVaultTest {

    @Test
    fun legacyPlaintextJwt_isReturnedAsIsWithoutDecrypting() {
        val jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ4In0.sig"
        val result = RelayCredentialVault.decryptWith(jwt) { error("must not be called for legacy plaintext") }
        assertEquals(jwt, result)
    }

    @Test
    fun base64Ciphertext_isNeverMistakenForLegacyPlaintext() {
        val stored = Base64.encodeToString(ByteArray(64) { it.toByte() }, Base64.NO_WRAP)
        assertFalse(RelayCredentialVault.isLegacyPlaintext(stored))
    }

    @Test
    fun ciphertext_isDecryptedWithInjectedDecryptor() {
        val stored = Base64.encodeToString("ENC".toByteArray(), Base64.NO_WRAP)
        val result = RelayCredentialVault.decryptWith(stored) { "plain.jwt.value".toByteArray() }
        assertEquals("plain.jwt.value", result)
    }

    @Test
    fun decryptFailure_isWrappedInDescriptiveIllegalStateException() {
        val stored = Base64.encodeToString("ENC".toByteArray(), Base64.NO_WRAP)
        try {
            RelayCredentialVault.decryptWith(stored) { throw javax.crypto.AEADBadTagException("tag mismatch") }
            fail("expected IllegalStateException")
        } catch (e: IllegalStateException) {
            assertTrue(e.message!!.contains("relay JWT"))
        }
    }
}
