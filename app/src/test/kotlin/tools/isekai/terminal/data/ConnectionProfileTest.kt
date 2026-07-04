package tools.isekai.terminal.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.tssh_core.SshAuth

/**
 * [ConnectionProfile]の変換関数([toSshConfig]及びそれが使う`toJumpConfigOrNull`)を、
 * UI/ViewModelを介さず直接検証する。
 */
class ConnectionProfileTest {

    private fun profile() = ConnectionProfile(
        label = "web", host = "example.com", port = 2222,
        username = "deploy", authType = "password",
    )

    private val auth = SshAuth.Password("secret")

    // ── toJumpConfigOrNull（toSshConfig経由で間接的に検証）────────────────

    @Test fun `toSshConfig has no jump when usesJumpHost is false`() {
        val config = profile().toSshConfig(auth, jumpAuth = SshAuth.Password("jump-secret"))
        assertNull(config.jump)
    }

    @Test fun `toSshConfig has no jump when jumpAuth is null even if jump host is configured`() {
        val withJump = profile().copy(jumpHost = "bastion.example.com", jumpUsername = "jumper")
        val config = withJump.toSshConfig(auth, jumpAuth = null)
        assertNull(config.jump)
    }

    @Test fun `toSshConfig maps jump fields when both jump host and jumpAuth are present`() {
        val withJump = profile().copy(
            jumpHost = "bastion.example.com", jumpPort = 2200, jumpUsername = "jumper",
        )
        val jumpAuth = SshAuth.Password("jump-secret")
        val config = withJump.toSshConfig(auth, jumpAuth = jumpAuth)
        assertEquals("bastion.example.com", config.jump?.host)
        assertEquals(2200.toUShort(), config.jump?.port)
        assertEquals("jumper", config.jump?.username)
        assertEquals(jumpAuth, config.jump?.auth)
    }

    // ── usesJumpHost（純粋な算出プロパティ）────────────────────────────

    @Test fun `usesJumpHost is false when jumpHost is null or blank`() {
        assertFalse(profile().usesJumpHost)
        assertFalse(profile().copy(jumpHost = "  ").usesJumpHost)
    }

    @Test fun `usesJumpHost is true when jumpHost is set`() {
        assertTrue(profile().copy(jumpHost = "bastion.example.com").usesJumpHost)
    }
}
