package tools.isekai.terminal.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.tssh_core.SshAuth

/**
 * [ConnectionProfile]の変換関数([toSshConfig]/[toIsekaiStunP2pConfig]/[toIsekaiLinkRelayConfig]、
 * 及びそれらが共通で使う`toJumpConfigOrNull`)を、UI/ViewModelを介さず直接検証する。
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

    // ── toHelperQuicConfig ───────────────────────────────────────────────

    @Test fun `toHelperQuicConfig maps ssh connection fields`() {
        val config = profile().toHelperQuicConfig(auth)
        assertEquals("example.com", config.sshHost)
        assertEquals(2222.toUShort(), config.sshPort)
        assertEquals("deploy", config.username)
        assertEquals(auth, config.auth)
    }

    @Test fun `toHelperQuicConfig maps helperBindPort to bindPort when set`() {
        val config = profile().copy(helperBindPort = 45900).toHelperQuicConfig(auth)
        assertEquals(45900.toUShort(), config.bindPort)
    }

    @Test fun `toHelperQuicConfig maps null helperBindPort to null bindPort`() {
        val config = profile().toHelperQuicConfig(auth)
        assertNull(config.bindPort)
    }

    // ── toIsekaiStunP2pConfig ────────────────────────────────────────────

    @Test fun `toIsekaiStunP2pConfig maps ssh connection fields`() {
        val config = profile().toIsekaiStunP2pConfig(auth)
        assertEquals("example.com", config.sshHost)
        assertEquals(2222.toUShort(), config.sshPort)
        assertEquals("deploy", config.username)
        assertEquals(auth, config.auth)
        assertEquals(80u, config.cols)
        assertEquals(24u, config.rows)
    }

    @Test fun `toIsekaiStunP2pConfig uses configured stunServer verbatim`() {
        val config = profile().copy(stunServer = "stun.example.com:3478").toIsekaiStunP2pConfig(auth)
        assertEquals("stun.example.com:3478", config.stunServer)
    }

    @Test fun `toIsekaiStunP2pConfig falls back to DEFAULT_STUN_SERVER when stunServer is null`() {
        val config = profile().toIsekaiStunP2pConfig(auth)
        assertEquals(ConnectionProfile.DEFAULT_STUN_SERVER, config.stunServer)
    }

    @Test fun `toIsekaiStunP2pConfig falls back to DEFAULT_STUN_SERVER when stunServer is blank`() {
        val config = profile().copy(stunServer = "   ").toIsekaiStunP2pConfig(auth)
        assertEquals(ConnectionProfile.DEFAULT_STUN_SERVER, config.stunServer)
    }

    @Test fun `toIsekaiStunP2pConfig has no jump when jumpAuth is not supplied`() {
        val config = profile().toIsekaiStunP2pConfig(auth)
        assertNull(config.jump)
    }

    // ── toIsekaiLinkRelayConfig ──────────────────────────────────────────

    @Test fun `toIsekaiLinkRelayConfig maps ssh connection fields`() {
        val withRelay = profile().copy(
            relayAddr = "relay.example.com:443",
            relaySni = "relay.example.com",
            relayJwt = "eyJhbGciOiJSUzI1NiJ9.test.sig",
        )
        val config = withRelay.toIsekaiLinkRelayConfig(auth)
        assertEquals("example.com", config.sshHost)
        assertEquals(2222.toUShort(), config.sshPort)
        assertEquals("deploy", config.username)
        assertEquals(auth, config.auth)
    }

    @Test fun `toIsekaiLinkRelayConfig maps relay fields verbatim`() {
        val withRelay = profile().copy(
            relayAddr = "relay.example.com:443",
            relaySni = "relay.example.com",
            relayJwt = "eyJhbGciOiJSUzI1NiJ9.test.sig",
        )
        val config = withRelay.toIsekaiLinkRelayConfig(auth)
        assertEquals("relay.example.com:443", config.relayAddr)
        assertEquals("relay.example.com", config.relaySni)
        assertEquals("eyJhbGciOiJSUzI1NiJ9.test.sig", config.relayJwt)
    }

    @Test fun `toIsekaiLinkRelayConfig maps missing relay fields to empty strings`() {
        val config = profile().toIsekaiLinkRelayConfig(auth)
        assertEquals("", config.relayAddr)
        assertEquals("", config.relaySni)
        assertEquals("", config.relayJwt)
    }

    // ── toMultipathHelperQuicConfig ───────────────────────────────────────

    @Test fun `toMultipathHelperQuicConfig maps ssh connection and direct_host fields`() {
        val config = profile().copy(directAddress = "203.0.113.5:45823").toMultipathHelperQuicConfig(auth)
        assertEquals("example.com", config.sshHost)
        assertEquals(2222.toUShort(), config.sshPort)
        assertEquals("203.0.113.5:45823", config.directHost)
    }

    @Test fun `toMultipathHelperQuicConfig maps helperBindPort to bindPort when set`() {
        val config = profile().copy(helperBindPort = 45900).toMultipathHelperQuicConfig(auth)
        assertEquals(45900.toUShort(), config.bindPort)
    }

    @Test fun `toMultipathHelperQuicConfig maps null helperBindPort to null bindPort`() {
        val config = profile().toMultipathHelperQuicConfig(auth)
        assertNull(config.bindPort)
    }

    // ── hasRelayConfig / usesJumpHost（純粋な算出プロパティ）──────────────

    @Test fun `hasRelayConfig is false when no relay fields are set`() {
        assertFalse(profile().hasRelayConfig)
    }

    @Test fun `hasRelayConfig is false when relay fields are blank strings`() {
        val withBlankRelay = profile().copy(relayAddr = "  ", relaySni = "  ", relayJwt = "  ")
        assertFalse(withBlankRelay.hasRelayConfig)
    }

    @Test fun `hasRelayConfig is true only when all three relay fields are set`() {
        val complete = profile().copy(
            relayAddr = "relay.example.com:443",
            relaySni = "relay.example.com",
            relayJwt = "eyJhbGciOiJSUzI1NiJ9.test.sig",
        )
        assertTrue(complete.hasRelayConfig)
    }

    @Test fun `usesJumpHost is false when jumpHost is null or blank`() {
        assertFalse(profile().usesJumpHost)
        assertFalse(profile().copy(jumpHost = "  ").usesJumpHost)
    }

    @Test fun `usesJumpHost is true when jumpHost is set`() {
        assertTrue(profile().copy(jumpHost = "bastion.example.com").usesJumpHost)
    }
}
