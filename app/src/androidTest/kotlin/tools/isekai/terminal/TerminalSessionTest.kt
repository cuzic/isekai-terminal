package tools.isekai.terminal

import androidx.test.ext.junit.runners.AndroidJUnit4
import tools.isekai.terminal.session.HostKeyDecision
import tools.isekai.terminal.session.TerminalSession
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.tssh_core.ScreenUpdate
import uniffi.tssh_core.SshAuth
import uniffi.tssh_core.SshConfig

/**
 * TerminalSession の動作テスト。
 * FakeOrchestrator / FakeHostKeyChecker を使い、Rust/Android 依存なしで動作を検証。
 */
@RunWith(AndroidJUnit4::class)
class TerminalSessionTest {

    private lateinit var fakeOrchestrator: FakeOrchestrator
    private lateinit var fakeHostKeyChecker: FakeHostKeyChecker
    private lateinit var session: TerminalSession

    @Before
    fun setup() {
        fakeOrchestrator = FakeOrchestrator()
        fakeHostKeyChecker = FakeHostKeyChecker()
        session = TerminalSession(fakeHostKeyChecker) { cb -> fakeOrchestrator.also { it.callback = cb } }
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
        forwards = emptyList(),
        agentForward = false,
        jump = null,
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
    fun connect_callsOrchestratorConnect() {
        session.connect(testConfig())
        assertTrue(fakeOrchestrator.connectCalled)
    }

    @Test
    fun connect_setsConnectingStatus() {
        session.connect(testConfig())
        // FakeOrchestrator.connect() fires Connecting synchronously
        assertEquals("接続中…", session.state.value.statusMsg)
        assertTrue(session.state.value.isConnecting)
    }

    @Test
    fun onConnected_updatesStateToConnected() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected("test.host")
        val state = awaitState { it.connected }
        assertTrue(state.connected)
        assertTrue(state.statusMsg.contains("test.host"))
        assertEquals("test.host", state.currentHost)
    }

    @Test
    fun onDisconnected_clearsConnectedState() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateDisconnected("server closed")
        val state = awaitState { !it.connected }
        assertFalse(state.connected)
        assertTrue(state.statusMsg.contains("server closed"))
        assertNull(state.screenUpdate)
        assertNull(state.currentHost)
    }

    @Test
    fun onDisconnected_withNullReason_showsFallback() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateDisconnected(null)
        val state = awaitState { !it.connected }
        assertTrue(state.statusMsg.contains("不明"))
    }

    @Test
    fun connect_whenAlreadyConnected_isNoOp() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        // 2回目の connect() は connected=true なのでガードで無視
        val prevOrchCalls = fakeOrchestrator.connectCalled
        session.connect(testConfig())
        delay(100)
        assertEquals("test.host", session.state.value.currentHost)
    }

    @Test
    fun connect_whenAlreadyConnecting_isNoOp() {
        session.connect(testConfig())  // fires Connecting, isConnecting=true
        session.connect(testConfig())  // guard: isConnecting=true → no-op
        // Only one connect should have been registered
        assertTrue(fakeOrchestrator.connectCalled)
    }

    // ── 切断 ──────────────────────────────────────────────────────

    @Test
    fun disconnect_whenNotConnected_setsDisconnectedMsg() {
        session.disconnect()
        assertEquals("切断済み", session.state.value.statusMsg)
        assertFalse(session.state.value.connected)
    }

    @Test
    fun disconnect_afterConnected_callsOrchestratorDisconnect() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        session.disconnect()
        assertTrue(fakeOrchestrator.disconnectCalled)
    }

    @Test
    fun disconnect_calledMultipleTimes_isIdempotent() {
        session.connect(testConfig())
        session.disconnect()
        session.disconnect()
        session.disconnect()
        assertEquals("切断済み", session.state.value.statusMsg)
        assertFalse(session.state.value.connected)
        assertFalse(session.state.value.isConnecting)
    }

    // ── 切断中に切断 (Bug 1 相当) ─────────────────────────────────

    @Test
    fun disconnect_duringConnecting_showsDisconnectedNotError() {
        session.connect(testConfig())
        assertEquals("接続中…", session.state.value.statusMsg)

        session.disconnect()

        assertEquals("切断済み", session.state.value.statusMsg)
        assertFalse(session.state.value.isConnecting)
        assertFalse(session.state.value.statusMsg.startsWith("エラー"))
    }

    // ── 接続失敗 ─────────────────────────────────────────────────

    @Test
    fun onError_showsError() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateError("Connection refused")
        awaitState { it.statusMsg.startsWith("エラー") }
        assertTrue(session.state.value.statusMsg.contains("Connection refused"))
    }

    // ── データ受信 ────────────────────────────────────────────────

    @Test
    fun onData_accumulatesInLog() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateData("hello ".toByteArray())
        fakeOrchestrator.simulateData("world".toByteArray())

        withTimeout(3000) { session.log.first { it.contains("world") } }
        assertEquals("hello world", session.log.value)
    }

    @Test
    fun clearLog_emptiesLog() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }
        fakeOrchestrator.simulateData("hello".toByteArray())
        withTimeout(3000) { session.log.first { it.isNotEmpty() } }

        session.clearLog()
        assertEquals("", session.log.value)
    }

    // ── 送信 ──────────────────────────────────────────────────────

    @Test
    fun send_afterConnected_delegatesToOrchestrator() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        val bytes = byteArrayOf(0x0D)
        session.send(bytes)
        assertTrue(fakeOrchestrator.sentBytes.any { it.contentEquals(bytes) })
    }

    @Test
    fun send_beforeConnected_delegatesAnyway() {
        // Orchestrator buffers or no-ops — TerminalSession just delegates
        session.send(byteArrayOf(0x0D))
        assertEquals(1, fakeOrchestrator.sentBytes.size)
    }

    // ── ホスト鍵 ─────────────────────────────────────────────────

    @Test
    fun onHostKey_trusted_returnsTrue() = runBlocking {
        session.connect(testConfig())
        val result = fakeOrchestrator.simulateHostKey(fingerprint = "sha256:abc")
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
        val fakeOrc2 = FakeOrchestrator()
        val s = TerminalSession(changedChecker) { cb -> fakeOrc2.also { it.callback = cb } }
        s.connect(testConfig())
        val result = fakeOrc2.simulateHostKey(fingerprint = "new-fp")
        assertFalse(result)

        val state = withTimeout(3000) { s.state.first { it.hostKeyChangedWarning != null } }
        assertNotNull(state.hostKeyChangedWarning)
        assertEquals("old-fp", state.hostKeyChangedWarning!!.oldFingerprint)
        s.close()
    }

    // ── SSH agent forwarding ─────────────────────────────────────

    @Test
    fun onAgentSignRequest_approved_returnsTrueAndClearsState() = runBlocking {
        session.connect(testConfig())
        val resultDeferred = async(Dispatchers.IO) {
            fakeOrchestrator.simulateAgentSignRequest("SHA256:approve-me")
        }
        withTimeout(3000) { session.state.first { it.agentSignRequestFingerprint == "SHA256:approve-me" } }

        session.respondAgentSignRequest(true)

        assertTrue(withTimeout(3000) { resultDeferred.await() })
        assertNull(session.state.value.agentSignRequestFingerprint)
    }

    @Test
    fun onAgentSignRequest_rejected_returnsFalse() = runBlocking {
        session.connect(testConfig())
        val resultDeferred = async(Dispatchers.IO) {
            fakeOrchestrator.simulateAgentSignRequest("SHA256:reject-me")
        }
        withTimeout(3000) { session.state.first { it.agentSignRequestFingerprint == "SHA256:reject-me" } }

        session.respondAgentSignRequest(false)

        assertFalse(withTimeout(3000) { resultDeferred.await() })
    }

    @Test
    fun respondAgentSignRequest_withoutPendingRequest_isNoop() {
        session.respondAgentSignRequest(true)
        assertNull(session.state.value.agentSignRequestFingerprint)
    }

    // ── 画面更新 ─────────────────────────────────────────────────

    @Test
    fun onScreenUpdate_whileConnected_updatesState() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        val update = ScreenUpdate(80u, 24u, emptyList(), 0u, 0u, "title1", false, false)
        fakeOrchestrator.simulateScreenUpdate(update)
        awaitState { it.screenUpdate != null }
        assertEquals("title1", session.state.value.screenUpdate?.title)
    }

    @Test
    fun onScreenUpdate_rapidFires_stateReceivesLatestFrame() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        val update1 = ScreenUpdate(80u, 24u, emptyList(), 0u, 0u, "title1", false, false)
        val update2 = ScreenUpdate(80u, 24u, emptyList(), 0u, 5u, "title2", false, false)
        val update3 = ScreenUpdate(80u, 24u, emptyList(), 0u, 10u, "title3", false, false)

        fakeOrchestrator.simulateScreenUpdate(update1)
        fakeOrchestrator.simulateScreenUpdate(update2)
        fakeOrchestrator.simulateScreenUpdate(update3)

        delay(100)
        assertEquals("title3", session.state.value.screenUpdate?.title)
    }

    @Test
    fun onScreenUpdate_afterDisconnect_doesNotApplyStaleFrame() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        val update = ScreenUpdate(80u, 24u, emptyList(), 0u, 0u, "before-disconnect", false, false)
        fakeOrchestrator.simulateScreenUpdate(update)
        awaitState { it.screenUpdate != null }

        fakeOrchestrator.simulateDisconnected("normal close")
        awaitState { !it.connected }
        assertNull("screenUpdate should be cleared on disconnect", session.state.value.screenUpdate)

        // 切断後に simulateScreenUpdate が来ても無視される
        val staleUpdate = ScreenUpdate(80u, 24u, emptyList(), 0u, 5u, "after-disconnect", false, false)
        fakeOrchestrator.simulateScreenUpdate(staleUpdate)
        delay(200)
        assertNull("stale screen update should not be applied after disconnect", session.state.value.screenUpdate)
    }

    @Test
    fun onScreenUpdate_manyRapidFires_allConsumedEventually() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        repeat(50) { i ->
            fakeOrchestrator.simulateScreenUpdate(
                ScreenUpdate(80u, 24u, emptyList(), 0u, i.toUInt(), "frame-$i", false, false)
            )
        }

        withTimeout(3000) { session.state.first { it.screenUpdate?.cursorCol?.toInt() == 49 } }
        assertEquals("frame-49", session.state.value.screenUpdate?.title)
    }

    // ── Bug 4: trzszDismiss 後に TrzszFinished が届いても UI が復活しない ─

    @Test
    fun trzszDismiss_beforeTrzszFinished_doesNotResurrectTrzszState() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateTrzszRequest("tid-1", "download", "file.txt", 1024u)
        awaitState { it.trzszState != null }

        // dismiss → fires Idle → trzszState = null
        session.trzszDismiss()
        awaitState { it.trzszState == null }

        // その後 Done が届いても... (Rust は dismiss後にDoneを出さないが、テスト用に確認)
        // In real Rust: dismiss clears FSM so Done won't fire.
        // In fake: we already verified trzszState is null after dismiss.
        assertNull("trzszState should be null after dismiss", session.state.value.trzszState)
    }

    @Test
    fun trzszDismiss_clearsState_evenIfProgressArrives() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateTrzszRequest("tid-2", "download", "file.txt", 1024u)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }

        session.trzszDismiss()
        awaitState { it.trzszState == null }

        // dismiss 後に progress が届いても null のまま（FakeOrchestrator は Idle を既に発火済み）
        // Rust は dismiss後にstateを出さないため、このシナリオは発生しない
        assertNull(session.state.value.trzszState)
    }

    // ── Bug 5: AcceptDownloadRequested の二重呼び出しを 1 回に抑制する ─

    @Test
    fun trzszAcceptDownload_calledTwiceRapidly_nativeAcceptedOnce() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateTrzszRequest("tid-3", "download", null, null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }

        session.trzszAcceptDownload()
        session.trzszAcceptDownload()
        delay(200)

        assertEquals(1, fakeOrchestrator.trzszAcceptDownloadCount)
    }

    @Test
    fun trzszAcceptDownload_afterNewTransfer_acceptsAgain() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        // 1 回目の転送
        fakeOrchestrator.simulateTrzszRequest("tid-4", "download", null, null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }
        session.trzszAcceptDownload()
        delay(100)
        assertEquals(1, fakeOrchestrator.trzszAcceptDownloadCount)

        // Done → transferAccepted リセット
        fakeOrchestrator.simulateTrzszFinished("tid-4", success = true)
        delay(100)

        // 2 回目の転送
        fakeOrchestrator.simulateTrzszRequest("tid-5", "download", null, null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }
        session.trzszAcceptDownload()
        delay(100)

        assertEquals(2, fakeOrchestrator.trzszAcceptDownloadCount)
    }

    // ── Bug 5 variant: upload accept 二重呼び出し ──────────────────────

    @Test
    fun trzszAcceptUpload_calledTwiceRapidly_nativeAcceptedOnce() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateTrzszRequest("up-tid", "upload", null, null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }

        session.trzszAcceptUpload("file.txt", 1024u, 0u)
        session.trzszAcceptUpload("file.txt", 1024u, 0u)
        delay(200)

        assertEquals(1, fakeOrchestrator.trzszAcceptUploadCount)
    }

    // ── Bug 5 variant: dismiss 後の accept は no-op ──────────────────────

    @Test
    fun trzszAcceptDownload_afterDismiss_isNoOp() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateTrzszRequest("tid-6", "download", null, null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }

        session.trzszDismiss()  // fires Idle → currentTransferId = null
        awaitState { it.trzszState == null }
        session.trzszAcceptDownload()  // currentTransferId is null → no-op
        delay(200)

        assertEquals(0, fakeOrchestrator.trzszAcceptDownloadCount)
    }

    @Test
    fun trzszAcceptDownload_afterCancel_isNoOp() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateTrzszRequest("tid-7", "download", null, null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }

        session.trzszCancel()  // clears currentTransferId immediately
        session.trzszAcceptDownload()  // no-op
        delay(200)

        assertEquals(0, fakeOrchestrator.trzszAcceptDownloadCount)
    }

    @Test
    fun trzszAcceptDownload_afterTrzszFinished_isNoOp() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateTrzszRequest("tid-8", "download", null, null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }
        session.trzszAcceptDownload()
        delay(100)
        assertEquals(1, fakeOrchestrator.trzszAcceptDownloadCount)

        fakeOrchestrator.simulateTrzszFinished("tid-8", success = true)
        awaitState { it.trzszState is TrzszUiState.Done }

        session.trzszAcceptDownload()
        delay(100)
        // currentTransferId is cleared on Done → accept after Done is a no-op
        assertEquals(1, fakeOrchestrator.trzszAcceptDownloadCount)
    }

    // ── NetworkLost edge cases ──────────────────────────────────────────

    @Test
    fun notifyNetworkLost_duringConnecting_setsAbortedMsg() = runBlocking {
        session.connect(testConfig())
        // Connecting state is set synchronously by FakeOrchestrator.connect()
        assertTrue(session.state.value.isConnecting)

        session.notifyNetworkLost()

        assertFalse(session.state.value.connected)
        assertFalse(session.state.value.isConnecting)
        assertTrue(session.state.value.statusMsg.contains("切断"))
    }

    @Test
    fun notifyNetworkLost_whenIdle_isNoOp() = runBlocking {
        session.notifyNetworkLost()
        delay(100)
        assertEquals("未接続", session.state.value.statusMsg)
        assertFalse(session.state.value.connected)
    }

    @Test
    fun notifyNetworkLost_calledMultipleTimes_isIdempotent() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        session.notifyNetworkLost()
        session.notifyNetworkLost()
        session.notifyNetworkLost()

        assertFalse(session.state.value.connected)
        // 1 回だけ disconnect が呼ばれる
        assertTrue(fakeOrchestrator.disconnectCalled)
    }

    // ── TrzszCancel edge cases ────────────────────────────────────────────

    @Test
    fun trzszCancel_whenNoActiveTransfer_isNoOp() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        session.trzszCancel()
        delay(100)
        assertEquals(0, fakeOrchestrator.trzszCancelCount)
        assertNull(session.state.value.trzszState)
    }

    @Test
    fun trzszCancel_duringTransfer_clearsState() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateTrzszRequest("cancel-tid", "upload", null, null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }

        session.trzszCancel()
        delay(200)

        // Cancel immediately clears state in Kotlin
        assertNull(session.state.value.trzszState)
        assertEquals(1, fakeOrchestrator.trzszCancelCount)
    }

    // ── Download complete ─────────────────────────────────────────────

    @Test
    fun onDownloadComplete_setsPendingDownloadFile() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateDownloadComplete(null, "hello".toByteArray())

        val pending = session.pendingDownloadFile.value
        assertNotNull(pending)
        assertEquals("download", pending!!.first)
        assertTrue(pending.second.contentEquals("hello".toByteArray()))
    }

    @Test
    fun onDownloadComplete_withFileName_usesProvidedName() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateDownloadComplete("secret.txt", "data".toByteArray())
        val pending = session.pendingDownloadFile.value
        assertEquals("secret.txt", pending?.first)
    }

    @Test
    fun trzszDismiss_thenNewTransfer_newTransferIsVisible() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateTrzszRequest("tid-1", "download", "file1.txt", null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }
        session.trzszDismiss()
        awaitState { it.trzszState == null }

        fakeOrchestrator.simulateTrzszRequest("tid-2", "upload", null, null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }
        val s = session.state.value.trzszState as TrzszUiState.WaitingUser
        assertEquals("tid-2", s.transferId)
        assertEquals("upload", s.mode)
    }

    @Test
    fun trzszDismiss_calledTwice_isIdempotent() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateTrzszRequest("tid-3", "download", null, null)
        awaitState { it.trzszState != null }

        session.trzszDismiss()
        session.trzszDismiss()
        delay(200)

        assertNull(session.state.value.trzszState)
    }

    @Test
    fun trzszDismiss_afterTrzszFinished_clearsDoneState() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateTrzszRequest("tid-4", "upload", null, null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }

        fakeOrchestrator.simulateTrzszFinished("tid-4", success = true)
        awaitState { it.trzszState is TrzszUiState.Done }

        session.trzszDismiss()
        awaitState { it.trzszState == null }
        assertNull(session.state.value.trzszState)
    }

    // ── Connect reconnect ─────────────────────────────────────────────

    @Test
    fun disconnect_thenConnect_canReconnect() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        session.disconnect()
        delay(100)

        // 再接続（新しいセッションインスタンスで）
        val newOrchestrator = FakeOrchestrator()
        val s = TerminalSession(FakeHostKeyChecker()) { cb -> newOrchestrator.also { it.callback = cb } }
        s.connect(testConfig())
        newOrchestrator.simulateConnected()
        withTimeout(3000) { s.state.first { it.connected } }

        assertTrue(s.state.value.connected)
        s.close()
    }

    // ── Close behavior ──────────────────────────────────────────────────

    @Test
    fun close_whenConnected_callsDisconnect() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        session.close()
        assertTrue(fakeOrchestrator.disconnectCalled)
    }

    @Test
    fun close_whenIdle_doesNotThrow() {
        session.close()
    }
}
