package tools.isekai.terminal

import tools.isekai.terminal.data.AuthType
import tools.isekai.terminal.session.AuthValidation
import tools.isekai.terminal.session.AuthValidator
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class AuthValidatorTest {

    // ── password ──────────────────────────────────────────────────

    @Test
    fun `password auth with valid password returns Password`() {
        val result = AuthValidator.validate("password", "secret", null)
        assertEquals(AuthValidation.Password("secret"), result)
    }

    @Test
    fun `password auth with empty password returns Error`() {
        val result = AuthValidator.validate("password", "", null)
        assertEquals(AuthValidation.Error("パスワードが必要です"), result)
    }

    @Test
    fun `password auth with null password returns Error`() {
        val result = AuthValidator.validate("password", null, null)
        assertEquals(AuthValidation.Error("パスワードが必要です"), result)
    }

    // ── key ───────────────────────────────────────────────────────

    @Test
    fun `key auth with keyId returns PublicKey`() {
        val result = AuthValidator.validate("key", null, 42L)
        assertEquals(AuthValidation.PublicKey(42L), result)
    }

    @Test
    fun `key auth with null keyId returns Error`() {
        val result = AuthValidator.validate("key", null, null)
        assertEquals(AuthValidation.Error("鍵IDが未設定です"), result)
    }

    // ── unknown ────────────────────────────────────────────────────

    @Test
    fun `unknown authType returns Error with message`() {
        val result = AuthValidator.validate("agent", null, null)
        assertTrue(result is AuthValidation.Error)
        assertTrue((result as AuthValidation.Error).statusMsg.contains("未知の認証タイプ"))
    }

    // ── typed AuthType overload(String版と同じ検証結果になること)────────

    @Test
    fun `typed PASSWORD auth with valid password returns Password`() {
        val result = AuthValidator.validate(AuthType.PASSWORD, "secret", null)
        assertEquals(AuthValidation.Password("secret"), result)
    }

    @Test
    fun `typed PASSWORD auth with null password returns Error`() {
        val result = AuthValidator.validate(AuthType.PASSWORD, null, null)
        assertEquals(AuthValidation.Error("パスワードが必要です"), result)
    }

    @Test
    fun `typed KEY auth with keyId returns PublicKey`() {
        val result = AuthValidator.validate(AuthType.KEY, null, 42L)
        assertEquals(AuthValidation.PublicKey(42L), result)
    }

    @Test
    fun `typed KEY auth with null keyId returns Error`() {
        val result = AuthValidator.validate(AuthType.KEY, null, null)
        assertEquals(AuthValidation.Error("鍵IDが未設定です"), result)
    }

    @Test
    fun `String and typed overloads agree for every valid input`() {
        assertEquals(
            AuthValidator.validate("password", "secret", null),
            AuthValidator.validate(AuthType.PASSWORD, "secret", null),
        )
        assertEquals(
            AuthValidator.validate("key", null, 42L),
            AuthValidator.validate(AuthType.KEY, null, 42L),
        )
    }
}
