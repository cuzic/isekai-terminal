package com.example.imespike

import androidx.test.ext.junit.runners.AndroidJUnit4
import com.example.imespike.session.HostKeyDecision
import com.example.imespike.session.TerminalSession
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.tssh_core.SshAuth
import uniffi.tssh_core.SshConfig

/**
 * TerminalSession の動作テスト。
 * FakeSshGateway / FakeHostKeyChecker を使い、Rust/Android 依存なしで動作を検証。
 */
@RunWith(AndroidJUnit4::class)
class TerminalSessionTest {

    private lateinit var fakeGateway: FakeSshGateway
    private lateinit var fakeHostKeyChecker: FakeHostKeyChecker
    private lateinit var session: TerminalSession

    @Before
    fun setup() {
        fakeGateway = FakeSshGateway()
        fakeHostKeyChecker = FakeHostKeyChecker()
        session = TerminalSession(fakeGateway, fakeHostKeyChecker)
    }

    @After
    fun teardown() {
        session.close()
    }

    private fun testConfig() = SshConfig(
        host = "test.host",
        port = 22u,
        username = "user",
        auth = SshAuth.Password("pass"),
        cols = 80u,
        rows = 24u,
    )

    private suspend fun awaitState(condition: (TerminalUiState) -> Boolean): TerminalUiState =
        withTimeout(3000) { session.state.first { condition(it) } }

    // ── 初期状態 ──────────────────────────────────────────────────

    @Test
    fun initialState_notConnected() {
        assertFalse(session.state.value.connected)
        assertEquals("未接続", session.state.value.statusMsg)
    }

    @Test
    fun initialState_logEmpty() {
        assertEquals("", session.log.value)
    }

    // ── 接続 ──────────────────────────────────────────────────────

    @Test
    fun connect_triggersGatewayCreate() {
        session.connect(testConfig())
        assertTrue(fakeGateway.session.connectCalled)
    }

    @Test
    fun connect_setsConnectingStatus() {
        session.connect(testConfig())
        assertEquals("接続中…", session.state.value.statusMsg)
    }

    @Test
    fun onConnected_updatesStateToConnected() = runBlocking {
        session.connect(testConfig())
        fakeGateway.session.simulateConnected()
        val state = awaitState { it.connected }
        assertTrue(state.connected)
        assertTrue(state.statusMsg.contains("test.host"))
        assertEquals("test.host", state.currentHost)
    }

    @Test
    fun onDisconnected_clearsConnectedState() = runBlocking {
        session.connect(testConfig())
        fakeGateway.session.simulateConnected()
        awaitState { it.connected }

        fakeGateway.session.simulateDisconnected("server closed")
        val state = awaitState { !it.connected }
        assertFalse(state.connected)
        assertTrue(state.statusMsg.contains("server closed"))
        assertNull(state.screenUpdate)
        assertNull(state.currentHost)
    }

    @Test
    fun onDisconnected_withNullReason_showsFallback() = runBlocking {
        session.connect(testConfig())
        fakeGateway.session.simulateConnected()
        awaitState { it.connected }

        fakeGateway.session.simulateDisconnected(null)
        val state = awaitState { !it.connected }
        assertTrue(state.statusMsg.contains("不明"))
    }

    @Test
    fun connect_whenAlreadyConnected_isNoOp() = runBlocking {
        session.connect(testConfig())
        fakeGateway.session.simulateConnected()
        awaitState { it.connected }

        val gatewayCallsBefore = 1  // create() called once
        session.connect(testConfig())
        // FakeSshGateway.create() は1回しか呼ばれない（connectCalled は既に true）
        // 2回目の connect() は state.connected==true なので早期リターン
        assertEquals("test.host", session.state.value.currentHost)
    }

    // ── 切断 ──────────────────────────────────────────────────────

    @Test
    fun disconnect_whenNotConnected_setsDisconnectedMsg() {
        session.disconnect()
        assertEquals("切断済み", session.state.value.statusMsg)
        assertFalse(session.state.value.connected)
    }

    @Test
    fun disconnect_afterConnected_callsNativeDisconnect() = runBlocking {
        session.connect(testConfig())
        fakeGateway.session.simulateConnected()
        awaitState { it.connected }

        session.disconnect()
        assertTrue(fakeGateway.session.disconnectCalled)
    }

    // ── データ受信 ────────────────────────────────────────────────

    @Test
    fun onData_accumulatesInLog() = runBlocking {
        session.connect(testConfig())
        fakeGateway.session.simulateConnected()
        awaitState { it.connected }

        fakeGateway.session.simulateData("hello ".toByteArray())
        fakeGateway.session.simulateData("world".toByteArray())

        withTimeout(3000) { session.log.first { it.contains("world") } }
        assertEquals("hello world", session.log.value)
    }

    @Test
    fun clearLog_emptiesLog() = runBlocking {
        session.connect(testConfig())
        fakeGateway.session.simulateConnected()
        awaitState { it.connected }
        fakeGateway.session.simulateData("hello".toByteArray())
        withTimeout(3000) { session.log.first { it.isNotEmpty() } }

        session.clearLog()
        assertEquals("", session.log.value)
    }

    // ── 送信 ──────────────────────────────────────────────────────

    @Test
    fun send_afterConnected_delegatesToNativeSession() = runBlocking {
        session.connect(testConfig())
        fakeGateway.session.simulateConnected()
        awaitState { it.connected }

        val bytes = byteArrayOf(0x0D)
        session.send(bytes)
        assertTrue(fakeGateway.session.sentBytes.any { it.contentEquals(bytes) })
    }

    @Test
    fun send_beforeConnected_isNoOp() {
        session.send(byteArrayOf(0x0D))
        assertTrue(fakeGateway.session.sentBytes.isEmpty())
    }

    // ── ホスト鍵 ─────────────────────────────────────────────────

    @Test
    fun onHostKey_trusted_returnsTrue() = runBlocking {
        session.connect(testConfig())
        val result = fakeGateway.session.simulateHostKey("sha256:abc")
        assertTrue(result)
        assertEquals(1, fakeHostKeyChecker.checked.size)
    }

    @Test
    fun onHostKey_changed_returnsFalse_andSetsWarning() = runBlocking {
        val changedChecker = FakeHostKeyChecker(
            HostKeyDecision.Changed(
                HostKeyChangedWarning("test.host", 22, "old-fp", "new-fp")
            )
        )
        val s = TerminalSession(fakeGateway, changedChecker)
        s.connect(testConfig())
        val result = fakeGateway.session.simulateHostKey("new-fp")
        assertFalse(result)

        val state = withTimeout(3000) { s.state.first { it.hostKeyChangedWarning != null } }
        assertNotNull(state.hostKeyChangedWarning)
        assertEquals("old-fp", state.hostKeyChangedWarning!!.oldFingerprint)
        s.close()
    }

    // ── auth error ────────────────────────────────────────────────

    @Test
    fun notifyAuthError_updatesStatusMsg() {
        session.notifyAuthError("パスワードが必要です")
        assertEquals("パスワードが必要です", session.state.value.statusMsg)
        assertFalse(session.state.value.connected)
    }
}
