package tools.isekai.terminal.session

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import tools.isekai.terminal.TerminalUiState
import tools.isekai.terminal.TrzszUiState
import uniffi.isekai_terminal_core.ConnectionPublicState
import uniffi.isekai_terminal_core.ScreenUpdate

/**
 * [ConnectionStateMapper]は[TerminalSession]のRustコールバックから切り出した純粋な
 * 状態畳み込み関数(Android/UniFFIコールバック配線を介さず直接検証できる)。
 */
class ConnectionStateMapperTest {

    private fun screenUpdate() = ScreenUpdate(80u, 24u, emptyList(), 0u, 0u, null, false, false)

    @Test
    fun `Connecting sets isConnecting and clears connected reconnecting`() {
        val current = TerminalUiState(connected = true, isReconnecting = true, statusMsg = "old")
        val result = ConnectionStateMapper.apply(current, ConnectionPublicState.Connecting)
        assertTrue(result.isConnecting)
        assertEquals(false, result.connected)
        assertEquals(false, result.isReconnecting)
        assertEquals("接続中…", result.statusMsg)
    }

    @Test
    fun `Connected sets connected and currentHost`() {
        val current = TerminalUiState(isConnecting = true)
        val result = ConnectionStateMapper.apply(current, ConnectionPublicState.Connected("example.com"))
        assertTrue(result.connected)
        assertEquals(false, result.isConnecting)
        assertEquals(false, result.isReconnecting)
        assertEquals("example.com", result.currentHost)
        assertEquals("接続済み: example.com", result.statusMsg)
    }

    @Test
    fun `Disconnected clears connection fields and screen state`() {
        val current = TerminalUiState(
            connected = true, currentHost = "example.com",
            screenUpdate = screenUpdate(),
            trzszState = TrzszUiState.WaitingUser("t1", "upload", null, null),
        )
        val result = ConnectionStateMapper.apply(current, ConnectionPublicState.Disconnected("network lost"))
        assertEquals(false, result.connected)
        assertEquals(false, result.isConnecting)
        assertEquals(false, result.isReconnecting)
        assertNull(result.currentHost)
        assertNull(result.screenUpdate)
        assertNull(result.trzszState)
        assertEquals("切断: network lost", result.statusMsg)
    }

    @Test
    fun `Disconnected with null reason uses a generic message`() {
        val result = ConnectionStateMapper.apply(TerminalUiState(), ConnectionPublicState.Disconnected(null))
        assertEquals("切断済み (不明)", result.statusMsg)
    }

    @Test
    fun `Error clears isConnecting and isReconnecting but preserves connected`() {
        val current = TerminalUiState(connected = true)
        val result = ConnectionStateMapper.apply(current, ConnectionPublicState.Error("boom"))
        assertEquals(false, result.isConnecting)
        assertEquals(false, result.isReconnecting)
        assertTrue("Errorはconnectedを変更しないはず", result.connected)
        assertEquals("エラー: boom", result.statusMsg)
    }

    @Test
    fun `Reconnecting sets isReconnecting and clears screen state`() {
        val current = TerminalUiState(screenUpdate = screenUpdate(), currentHost = "example.com")
        val result = ConnectionStateMapper.apply(
            current,
            ConnectionPublicState.Reconnecting(elapsedSecs = 5u, timeoutSecs = 60u, reason = "network lost"),
        )
        assertTrue(result.isReconnecting)
        assertEquals(false, result.connected)
        assertEquals(false, result.isConnecting)
        assertNull(result.currentHost)
        assertNull(result.screenUpdate)
        assertEquals("再接続中… (5/60秒) [network lost]", result.statusMsg)
    }

    @Test
    fun `Reconnecting with null reason omits the bracketed suffix`() {
        val result = ConnectionStateMapper.apply(
            TerminalUiState(),
            ConnectionPublicState.Reconnecting(elapsedSecs = 1u, timeoutSecs = 60u, reason = null),
        )
        assertEquals("再接続中… (1/60秒)", result.statusMsg)
    }
}
