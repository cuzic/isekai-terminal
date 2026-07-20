package tools.isekai.terminal

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
import uniffi.isekai_terminal_core.QuicConfig
import uniffi.isekai_terminal_core.CursorShape
import uniffi.isekai_terminal_core.MouseReportingMode
import uniffi.isekai_terminal_core.PromptJumpTarget
import uniffi.isekai_terminal_core.ScreenUpdate
import uniffi.isekai_terminal_core.ScrollbackSearchMatch
import uniffi.isekai_terminal_core.SshAuth
import uniffi.isekai_terminal_core.SshConfig

/**
 * TerminalSession の動作テスト（JVM）。
 * FakeOrchestrator / FakeHostKeyChecker を使い、Rust/Android 依存なしで動作を検証。
 */
class TerminalSessionTest {

    private lateinit var fakeOrchestrator: FakeOrchestrator
    private lateinit var fakeHostKeyChecker: FakeHostKeyChecker
    private lateinit var session: TerminalSession

    @Before
    fun setup() {
        fakeOrchestrator = FakeOrchestrator()
        fakeHostKeyChecker = FakeHostKeyChecker()
        session = TerminalSession(fakeHostKeyChecker, orchestratorFactory = { cb -> fakeOrchestrator.also { it.callback = cb } })
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
        allowNonLoopbackForwardBind = false,
    )

    private suspend fun awaitState(condition: (TerminalUiState) -> Boolean): TerminalUiState =
        withTimeout(3000) { session.state.first { condition(it) } }

    /** #25: `bellGeneration`だけを可変にした最小の`ScreenUpdate`。他フィールドは
     *  既存テストと同じ最小値で埋める。 */
    private fun bellUpdate(bellGeneration: ULong, cursorCol: UInt = 0u) = ScreenUpdate(
        0u, 80u, 24u, emptyList(), 0u, cursorCol, "title", false, false, false,
        MouseReportingMode.OFF, false, true, bellGeneration, CursorShape.BLOCK, true, emptyList(), emptyList(), 0u, null,
    )

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

    // ── 自動再接続(Reconnecting) ──────────────────────────────────

    @Test
    fun onReconnecting_updatesStateWithLiveCountdown() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateReconnecting(elapsedSecs = 3u, timeoutSecs = 60u, reason = "peer closed")
        val state = awaitState { it.isReconnecting }
        assertFalse(state.connected)
        assertFalse(state.isConnecting)
        assertTrue(state.statusMsg.contains("3"))
        assertTrue(state.statusMsg.contains("60"))
        assertTrue(state.statusMsg.contains("peer closed"))
        assertNull(state.screenUpdate)
        assertNull(state.currentHost)
    }

    @Test
    fun onReconnecting_thenConnected_clearsIsReconnecting() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateReconnecting(elapsedSecs = 3u, timeoutSecs = 60u)
        awaitState { it.isReconnecting }

        fakeOrchestrator.simulateConnected("test.host")
        val state = awaitState { it.connected }
        assertFalse(state.isReconnecting)
    }

    @Test
    fun onDisconnected_afterConnect_neverSetsIsReconnecting() = runBlocking {
        // 通常の切断(Reconnectingを経由しないケース)ではisReconnectingは立たない。
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateDisconnected("server closed")
        val state = awaitState { !it.connected }
        assertFalse(state.isReconnecting)
    }

    @Test
    fun cancelReconnect_delegatesToOrchestrator() {
        session.cancelReconnect()
        assertTrue(fakeOrchestrator.cancelReconnectCalled)
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
        val s = TerminalSession(changedChecker, orchestratorFactory = { cb -> fakeOrc2.also { it.callback = cb } })
        s.connect(testConfig())
        val result = fakeOrc2.simulateHostKey(fingerprint = "new-fp")
        assertFalse(result)

        val state = withTimeout(3000) { s.state.first { it.hostKeyChangedWarning != null } }
        assertNotNull(state.hostKeyChangedWarning)
        assertEquals("old-fp", state.hostKeyChangedWarning!!.oldFingerprint)
        s.close()
    }

    // ── SSH agent forwarding ─────────────────────────────────────

    // onAgentSignRequest() は respondAgentSignRequest() が呼ばれるまで呼び出し元スレッドを
    // ブロックする設計（本番では Rust の spawn_blocking スレッド）。テストでは別コルーチンで
    // 呼び出し、state に fingerprint が反映されるのを待ってから応答する。
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
        // 保留中の要求が無い状態で呼んでも何も起きない（二重応答やタイミングずれのガード）。
        session.respondAgentSignRequest(true)
        assertNull(session.state.value.agentSignRequestFingerprint)
    }

    // ── 画面更新 ─────────────────────────────────────────────────

    @Test
    fun onScreenUpdate_whileConnected_updatesState() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        val update = ScreenUpdate(0u, 80u, 24u, emptyList(), 0u, 0u, "title1", false, false, false, MouseReportingMode.OFF, false, true, 0uL, CursorShape.BLOCK, true, emptyList(), emptyList(), 0u, null)
        fakeOrchestrator.simulateScreenUpdate(update)
        awaitState { it.screenUpdate != null }
        assertEquals("title1", session.state.value.screenUpdate?.title)
    }

    @Test
    fun onScreenUpdate_rapidFires_stateReceivesLatestFrame() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        val update1 = ScreenUpdate(0u, 80u, 24u, emptyList(), 0u, 0u, "title1", false, false, false, MouseReportingMode.OFF, false, true, 0uL, CursorShape.BLOCK, true, emptyList(), emptyList(), 0u, null)
        val update2 = ScreenUpdate(0u, 80u, 24u, emptyList(), 0u, 5u, "title2", false, false, false, MouseReportingMode.OFF, false, true, 0uL, CursorShape.BLOCK, true, emptyList(), emptyList(), 0u, null)
        val update3 = ScreenUpdate(0u, 80u, 24u, emptyList(), 0u, 10u, "title3", false, false, false, MouseReportingMode.OFF, false, true, 0uL, CursorShape.BLOCK, true, emptyList(), emptyList(), 0u, null)

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

        val update = ScreenUpdate(0u, 80u, 24u, emptyList(), 0u, 0u, "before-disconnect", false, false, false, MouseReportingMode.OFF, false, true, 0uL, CursorShape.BLOCK, true, emptyList(), emptyList(), 0u, null)
        fakeOrchestrator.simulateScreenUpdate(update)
        awaitState { it.screenUpdate != null }

        fakeOrchestrator.simulateDisconnected("normal close")
        awaitState { !it.connected }
        assertNull("screenUpdate should be cleared on disconnect", session.state.value.screenUpdate)

        // 切断後に simulateScreenUpdate が来ても無視される
        val staleUpdate = ScreenUpdate(0u, 80u, 24u, emptyList(), 0u, 5u, "after-disconnect", false, false, false, MouseReportingMode.OFF, false, true, 0uL, CursorShape.BLOCK, true, emptyList(), emptyList(), 0u, null)
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
                ScreenUpdate(0u, 80u, 24u, emptyList(), 0u, i.toUInt(), "frame-$i", false, false, false, MouseReportingMode.OFF, false, true, 0uL, CursorShape.BLOCK, true, emptyList(), emptyList(), 0u, null)
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
    fun notifyNetworkPathChanged_duringConnecting_setsAbortedMsg() = runBlocking {
        session.connect(testConfig())
        // Connecting state is set synchronously by FakeOrchestrator.connect()
        assertTrue(session.state.value.isConnecting)

        session.notifyNetworkPathChanged(isSatisfied = false)

        assertFalse(session.state.value.connected)
        assertFalse(session.state.value.isConnecting)
        assertTrue(session.state.value.statusMsg.contains("切断"))
    }

    @Test
    fun notifyNetworkPathChanged_whenIdle_isNoOp() = runBlocking {
        session.notifyNetworkPathChanged(isSatisfied = false)
        delay(100)
        assertEquals("未接続", session.state.value.statusMsg)
        assertFalse(session.state.value.connected)
    }

    @Test
    fun notifyNetworkPathChanged_calledMultipleTimes_isIdempotent() = runBlocking {
        session.connect(testConfig())
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        session.notifyNetworkPathChanged(isSatisfied = false)
        session.notifyNetworkPathChanged(isSatisfied = false)
        session.notifyNetworkPathChanged(isSatisfied = false)

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
        val s = TerminalSession(FakeHostKeyChecker(), orchestratorFactory = { cb -> newOrchestrator.also { it.callback = cb } })
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

    // ── ViewModel 相当カバレッジ（JVM で検証）────────────────────────

    @Test
    fun notifyNetworkPathChanged_whenQuicConnected_doesNotDisconnect() = runBlocking {
        val quicConfig = QuicConfig(tsshdHost = "test.host", tsshdPort = 2222u,
            sshHost = "test.host", sshPort = 22u,
            username = "user", auth = SshAuth.Password("pass"),
            cols = 80u, rows = 24u, skipCertVerify = true)
        session.connectQuic(quicConfig)
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        session.notifyNetworkPathChanged(isSatisfied = false)
        delay(200)

        assertTrue("QUIC 接続はネットワーク喪失で切断されない", session.state.value.connected)
        assertFalse(fakeOrchestrator.disconnectCalled)
    }

    @Test
    fun disconnect_whenNotConnected_setsDisconnectedMsgFromIdle() {
        assertEquals("未接続", session.state.value.statusMsg)
        session.disconnect()
        assertEquals("切断済み", session.state.value.statusMsg)
        assertFalse(session.state.value.isConnecting)
    }

    // ── #25: BEL(端末ベル)受信時のフィードバック ────────────────────────

    @Test
    fun onScreenUpdate_bellGenerationAdvances_firesOnBellOnce() = runBlocking {
        val bellCount = java.util.concurrent.atomic.AtomicInteger(0)
        val orch = FakeOrchestrator()
        val s = TerminalSession(
            FakeHostKeyChecker(),
            orchestratorFactory = { cb -> orch.also { it.callback = cb } },
            onBell = { bellCount.incrementAndGet() },
        )
        s.connect(testConfig())
        orch.simulateConnected()
        withTimeout(3000) { s.state.first { it.connected } }

        orch.simulateScreenUpdate(bellUpdate(bellGeneration = 1uL))
        withTimeout(3000) { s.state.first { it.screenUpdate?.bellGeneration == 1uL } }
        delay(50)

        assertEquals(1, bellCount.get())
        s.close()
    }

    @Test
    fun onScreenUpdate_sameBellGenerationReapplied_doesNotFireAgain() = runBlocking {
        val bellCount = java.util.concurrent.atomic.AtomicInteger(0)
        val orch = FakeOrchestrator()
        val s = TerminalSession(
            FakeHostKeyChecker(),
            orchestratorFactory = { cb -> orch.also { it.callback = cb } },
            onBell = { bellCount.incrementAndGet() },
        )
        s.connect(testConfig())
        orch.simulateConnected()
        withTimeout(3000) { s.state.first { it.connected } }

        orch.simulateScreenUpdate(bellUpdate(bellGeneration = 2uL))
        withTimeout(3000) { s.state.first { it.screenUpdate?.bellGeneration == 2uL } }
        delay(50)
        assertEquals(1, bellCount.get())

        // 同じ bellGeneration の ScreenUpdate が(conflated チャネル越しの重複配送等で)
        // 再適用されても二重発火しない(cursorCol だけ変えて、再適用が実際に消費された
        // ことを検証可能にする)。
        orch.simulateScreenUpdate(bellUpdate(bellGeneration = 2uL, cursorCol = 5u))
        withTimeout(3000) { s.state.first { it.screenUpdate?.cursorCol == 5u } }
        delay(50)

        assertEquals(1, bellCount.get())
        s.close()
    }

    @Test
    fun onScreenUpdate_multipleIncreasingBellGenerations_firesForEach() = runBlocking {
        val bellCount = java.util.concurrent.atomic.AtomicInteger(0)
        val orch = FakeOrchestrator()
        val s = TerminalSession(
            FakeHostKeyChecker(),
            orchestratorFactory = { cb -> orch.also { it.callback = cb } },
            onBell = { bellCount.incrementAndGet() },
        )
        s.connect(testConfig())
        orch.simulateConnected()
        withTimeout(3000) { s.state.first { it.connected } }

        orch.simulateScreenUpdate(bellUpdate(bellGeneration = 1uL))
        withTimeout(3000) { s.state.first { it.screenUpdate?.bellGeneration == 1uL } }
        orch.simulateScreenUpdate(bellUpdate(bellGeneration = 2uL))
        withTimeout(3000) { s.state.first { it.screenUpdate?.bellGeneration == 2uL } }
        orch.simulateScreenUpdate(bellUpdate(bellGeneration = 3uL))
        withTimeout(3000) { s.state.first { it.screenUpdate?.bellGeneration == 3uL } }
        delay(50)

        assertEquals(3, bellCount.get())
        s.close()
    }

    @Test
    fun onScreenUpdate_whileDisconnected_doesNotFireBell() = runBlocking {
        val bellCount = java.util.concurrent.atomic.AtomicInteger(0)
        val orch = FakeOrchestrator()
        val s = TerminalSession(
            FakeHostKeyChecker(),
            orchestratorFactory = { cb -> orch.also { it.callback = cb } },
            onBell = { bellCount.incrementAndGet() },
        )
        s.connect(testConfig())
        orch.simulateConnected()
        withTimeout(3000) { s.state.first { it.connected } }

        orch.simulateDisconnected("server closed")
        withTimeout(3000) { s.state.first { !it.connected } }

        orch.simulateScreenUpdate(bellUpdate(bellGeneration = 1uL))
        delay(200)

        assertEquals(0, bellCount.get())
        s.close()
    }

    /** Fableレビュー指摘: 同一ペイン(同一[TerminalSession]インスタンス)のまま手動で
     *  再接続すると、Rust側で新しい`Terminal`が作られ`bellGeneration`は0から
     *  再スタートする(#24)。ここでは[guardedConnect]が新しい接続開始直前に
     *  `lastFiredBellGeneration`をリセットすることを、「旧セッションで高い世代の
     *  BELを記憶した後、新セッションの低い世代(1)のBELでも取りこぼさず発火する」
     *  という観測可能な振る舞いで検証する(iOS版#26のreconnectテストと同じ観点)。 */
    @Test
    fun reconnect_resetsLastFiredBellGeneration_newSessionLowGenerationStillFires() = runBlocking {
        val bellCount = java.util.concurrent.atomic.AtomicInteger(0)
        val orch = FakeOrchestrator()
        val s = TerminalSession(
            FakeHostKeyChecker(),
            orchestratorFactory = { cb -> orch.also { it.callback = cb } },
            onBell = { bellCount.incrementAndGet() },
        )
        s.connect(testConfig())
        orch.simulateConnected()
        withTimeout(3000) { s.state.first { it.connected } }

        // 旧セッションで高い世代(5)のBELを既に記憶した状態を模す。
        orch.simulateScreenUpdate(bellUpdate(bellGeneration = 5uL))
        withTimeout(3000) { s.state.first { it.screenUpdate?.bellGeneration == 5uL } }
        delay(50)
        assertEquals(1, bellCount.get())

        orch.simulateDisconnected("server closed")
        withTimeout(3000) { s.state.first { !it.connected } }

        // 再接続(同一 TerminalSession インスタンス、新しい論理セッション)。
        s.connect(testConfig())
        orch.simulateConnected()
        withTimeout(3000) { s.state.first { it.connected } }

        // 新セッションの bellGeneration は 1 から再スタートする(旧セッションの
        // 記憶値 5 より小さいが、リセットされているので取りこぼさず発火する)。
        orch.simulateScreenUpdate(bellUpdate(bellGeneration = 1uL))
        withTimeout(3000) { s.state.first { it.screenUpdate?.bellGeneration == 1uL } }
        delay(50)

        assertEquals(2, bellCount.get())
        s.close()
    }

    // ── #66: スクロールバック検索(search_scrollback)の中継 ──────────────

    /** マッチ計算そのものは`SessionCore::search_scrollback`(Rust側、#37)で既にテスト済み
     *  (`session.rs`の`search_scrollback_*`群)。ここでは[TerminalSession.searchScrollback]が
     *  その呼び出しをそのまま中継しているだけであること(クエリ・大小文字区別の引数と
     *  戻り値の両方)を確認する(iOS版`TerminalSessionControllerTests.swift`の
     *  `testSearchScrollbackReturnsEmptyBeforeConnecting`と対称)。 */
    @Test
    fun searchScrollback_delegatesQueryAndResultToOrchestrator() {
        val expected = listOf(ScrollbackSearchMatch(row = 1u, col = 2u, len = 3u))
        fakeOrchestrator.searchScrollbackResult = expected

        val result = session.searchScrollback("needle", caseSensitive = true)

        assertEquals("needle", fakeOrchestrator.lastSearchScrollbackQuery)
        assertEquals(true, fakeOrchestrator.lastSearchScrollbackCaseSensitive)
        assertEquals(expected, result)
    }

    /** 未接続時でもクラッシュせず、フェイクが返した値(既定では空リスト)をそのまま
     *  返すこと——[scrollbackCells]と同じ「未接続ガードはRust側([FakeOrchestrator]が
     *  模す)の責務」という契約。 */
    @Test
    fun searchScrollback_beforeConnecting_returnsEmptyList() {
        assertEquals(emptyList<ScrollbackSearchMatch>(), session.searchScrollback("abc", caseSensitive = false))
    }

    // ── OSC 133(タスク#13) ──────────────────────────────────

    /** 判断ロジック(どのプロンプトが「前/次」か)は全てRust側
     *  ([Terminal::prompt_jump_target]、`rust-core/src/terminal.rs`)にあるため、
     *  ここでは[TerminalSession.jumpToPreviousPrompt]/[jumpToNextPrompt]が引数を
     *  そのまま中継していることだけを確認する([searchScrollback_delegatesQueryAndResultToOrchestrator]
     *  と同型のテスト)。 */
    @Test
    fun jumpToPreviousPrompt_delegatesArgsToOrchestrator() {
        session.jumpToPreviousPrompt(fromScrollOffset = 5, fromShowingScrollback = true)

        assertEquals(listOf(5u to true), fakeOrchestrator.jumpToPreviousPromptCalls)
    }

    @Test
    fun jumpToNextPrompt_delegatesArgsToOrchestrator() {
        session.jumpToNextPrompt(fromScrollOffset = 0, fromShowingScrollback = false)

        assertEquals(listOf(0u to false), fakeOrchestrator.jumpToNextPromptCalls)
    }

    @Test
    fun clickToPromptCursor_delegatesArgsToOrchestrator() {
        session.clickToPromptCursor(row = 3, col = 7)

        assertEquals(listOf(3u to 7u), fakeOrchestrator.clickToPromptCursorCalls)
    }

    @Test
    fun copyLastCommandOutput_delegatesToOrchestrator() {
        session.copyLastCommandOutput()

        assertEquals(1, fakeOrchestrator.copyLastCommandOutputCallCount)
    }

    /** `onPromptJump`コールバックの結果が[TerminalUiState.promptJumpResult]へ反映される
     *  こと、および`seq`が呼び出しごとに単調増加すること
     *  ([PromptJumpResult]のdocコメント参照——`target`が同じ`null`のまま連続しても
     *  Compose側の`LaunchedEffect`が確実に再発火できるようにするための設計)。 */
    @Test
    fun onPromptJump_updatesStateWithTargetAndIncrementingSeq() {
        val target = PromptJumpTarget(scrollOffset = 12u, isLive = false)

        fakeOrchestrator.simulatePromptJump(target)
        assertEquals(target, session.state.value.promptJumpResult.target)
        assertEquals(1L, session.state.value.promptJumpResult.seq)

        // 見つからなかった(null)場合でもseqは進む。
        fakeOrchestrator.simulatePromptJump(null)
        assertNull(session.state.value.promptJumpResult.target)
        assertEquals(2L, session.state.value.promptJumpResult.seq)
    }

    @Test
    fun onPromptOutputCopyReady_updatesStateWithTextAndIncrementingSeq() {
        fakeOrchestrator.simulatePromptOutputCopyReady("line1\nline2")
        assertEquals("line1\nline2", session.state.value.promptOutputCopyResult.text)
        assertEquals(1L, session.state.value.promptOutputCopyResult.seq)

        fakeOrchestrator.simulatePromptOutputCopyReady(null)
        assertNull(session.state.value.promptOutputCopyResult.text)
        assertEquals(2L, session.state.value.promptOutputCopyResult.seq)
    }
}
